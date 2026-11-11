//! Replays the govec `expr` vectors (tidb-rs-tracker `govec/domains/expr.go`): the plan-side
//! expression nodes of block D08.1 part 1 (Column, CorrelatedColumn, Constant, Schema) against
//! Go TiDB v8.5.8.
//!
//! `GOVEC_EXPR_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{IgnoreWarnHandler, SQLWarn, WarnHandler, new_static_warn_handler};
use tidb_base::errors::{RedactLogDisable, RedactLogEnable, RedactLogMarker};
use tidb_base::mysql::{self, SQLMode};
use tidb_chunk::{Chunk, Row, chunk};
use tidb_expression::*;
use tidb_parser::model::CIStr;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, FieldType,
};
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::{
    BuildContextRef, EmptyParamValues, new_simple_plan_column_id_allocator,
};
use tidb_sessionctx::exprstatic::{self as es, EvalCtxOption};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::context::Flags;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum, new_int_datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type::new_field_type;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::{BinaryJSON, parse_binary_json_from_string};
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, Time, new_time};
use tidb_types::vector::{parse_vector_float32, zero_copy_deserialize_vector_float32};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/expr.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---- helpers shared with the chunk / datum / exprctx replays -------------------------------

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

fn s_u64(v: &Value) -> u64 {
    v.as_str().unwrap().parse().unwrap()
}

fn s_i64(v: &Value) -> i64 {
    v.as_str().unwrap().parse().unwrap()
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

fn vec_bytes(bits: &[Value]) -> Vec<u8> {
    let mut b = (bits.len() as u32).to_le_bytes().to_vec();
    for x in bits {
        b.extend_from_slice(&(x.as_u64().unwrap() as u32).to_le_bytes());
    }
    b
}

/// The chunk domain's chunkTypeSpec: [tp, flag, collate, flen].
fn ft_of(spec: &Value) -> FieldType {
    let a = spec.as_array().unwrap();
    let tp = a[0].as_u64().unwrap() as u8;
    let mut ft = new_field_type(tp);
    ft.set_flag(a[1].as_u64().unwrap() as usize);
    ft.set_collate(a[2].as_str().unwrap());
    let flen = a[3].as_i64().unwrap() as isize;
    ft.set_flen(flen);
    if tp == mysql::TypeDuration {
        ft.set_decimal(flen % 7);
    }
    ft
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

fn kind(ft: &FieldType) -> &'static str {
    match ft.get_type() {
        mysql::TypeTiny
        | mysql::TypeShort
        | mysql::TypeInt24
        | mysql::TypeLong
        | mysql::TypeLonglong
        | mysql::TypeYear => {
            if mysql::has_unsigned_flag(ft.get_flag()) {
                "u64"
            } else {
                "i64"
            }
        }
        mysql::TypeFloat => "f32",
        mysql::TypeDouble => "f64",
        mysql::TypeNewDecimal => "dec",
        mysql::TypeDate | mysql::TypeDatetime | mysql::TypeTimestamp => "time",
        mysql::TypeDuration => "dur",
        mysql::TypeJSON => "json",
        mysql::TypeTiDBVectorFloat32 => "vec",
        mysql::TypeEnum | mysql::TypeSet => "nv",
        mysql::TypeNull => "null",
        _ => "bytes",
    }
}

/// govec's chunkAppend (row level).
fn append(c: &mut Chunk, ft: &FieldType, col: usize, v: &Value) {
    if v.is_null() {
        c.append_null(col);
        return;
    }
    match kind(ft) {
        "i64" => c.append_int64(col, s_i64(v)),
        "u64" => c.append_uint64(col, s_u64(v)),
        "f32" => c.append_float32(col, f32::from_bits(s_u64(v) as u32)),
        "f64" => c.append_float64(col, f64::from_bits(s_u64(v))),
        "dec" => c.append_my_decimal(col, &dec_of(v.as_str().unwrap())),
        "time" => {
            let a = v.as_array().unwrap();
            let t = new_time(
                CoreTime(s_u64(&a[0])),
                a[1].as_u64().unwrap() as u8,
                a[2].as_i64().unwrap() as isize,
            );
            c.append_time(col, t)
        }
        "dur" => {
            let a = v.as_array().unwrap();
            c.append_duration(
                col,
                Duration {
                    duration: s_i64(&a[0]),
                    fsp: a[1].as_i64().unwrap() as isize,
                },
            )
        }
        "json" => {
            let j = parse_binary_json_from_string(v.as_str().unwrap().as_bytes()).unwrap();
            c.append_json(col, &j)
        }
        "vec" => {
            let b = vec_bytes(v.as_array().unwrap());
            let (vv, _) = zero_copy_deserialize_vector_float32(&b).unwrap();
            c.append_vector_float32(col, &vv)
        }
        "nv" => {
            let a = v.as_array().unwrap();
            let name = unhex(a[0].as_str().unwrap());
            let val = a[1].as_u64().unwrap();
            if ft.get_type() == mysql::TypeEnum {
                c.append_enum(col, &Enum { name, value: val })
            } else {
                c.append_set(col, &Set { name, value: val })
            }
        }
        _ => {
            let b = unhex(v.as_str().unwrap());
            if ft.get_type() == mysql::TypeVarchar {
                c.append_string(col, &b)
            } else {
                c.append_bytes(col, &b)
            }
        }
    }
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
        dm::KindMinNotNull => d.set_min_not_null(),
        dm::KindMaxValue => d = dm::max_value_datum(),
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
        dm::KindRaw => d.set_raw(hex_opt(&p[1])),
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

fn time_out(t: &Time) -> Value {
    json!({ "raw": t.raw().0, "s": gs(&t.string()) })
}

/// exprctx's ecEvalOpts (the subset the expr domain uses).
fn eval_opts(v: &Value) -> Vec<EvalCtxOption> {
    let mut out: Vec<EvalCtxOption> = Vec::new();
    for o in v.as_array().unwrap() {
        let o = o.as_array().unwrap();
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
            "levels" => {
                let mut lm = [errctx::LevelError; 7];
                for (i, l) in o[1].as_array().unwrap().iter().enumerate() {
                    lm[i] = errctx::Level(l.as_u64().unwrap() as u8);
                }
                out.push(es::with_err_level_map(lm))
            }
            "params" => {
                let ps: Vec<_> = o[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| new_int_datum(v.as_i64().unwrap()))
                    .collect();
                out.push(es::with_param_list(&ps))
            }
            other => panic!("eval_opts: {other}"),
        }
    }
    out
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

// ---- the domain ------------------------------------------------------------------------------

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

fn ex_datum(d: &Datum) -> Value {
    let mut h = Vec::new();
    tidb_codec::hash_code(&mut h, d);
    json!([d.kind(), hex(&h), gs(d.collation().as_bytes())])
}

fn ex_eval_set(ft: &FieldType, k: u8, is_const: bool) -> Vec<&'static str> {
    let et = ft.eval_type();
    let mut set = vec!["eval"];
    let mut add = |m: &'static str| {
        if !set.contains(&m) {
            set.push(m)
        }
    };
    let hybrid = ft.hybrid();
    if et == ETInt
        || hybrid
        || (is_const
            && (k == dm::KindString || k == dm::KindBinaryLiteral || k == dm::KindMysqlBit))
    {
        add("int");
    }
    if et == ETReal || (is_const && (hybrid || k == dm::KindBinaryLiteral || k == dm::KindString)) {
        add("real");
    }
    if et == ETString || hybrid || is_const {
        add("string");
    }
    if et == ETDecimal || (is_const && (et == ETInt || et == ETReal || et == ETString)) {
        add("decimal");
    }
    if et == ETDatetime || et == ETTimestamp || (is_const && k == dm::KindMysqlTime) {
        add("time");
    }
    if et == ETDuration || (is_const && k == dm::KindMysqlDuration) {
        add("duration");
    }
    if et == ETJson || (is_const && k == dm::KindMysqlJSON) {
        add("json");
    }
    if et == ETVectorFloat32 || (is_const && k == dm::KindVectorFloat32) {
        add("vector");
    }
    set
}

fn ex_evals(
    ctx: &Arc<es::EvalContext>,
    e: &Expr,
    st: &EvalState,
    row: Row<'_>,
    set: &[&str],
) -> serde_json::Map<String, Value> {
    let c: &dyn EvalContext = &**ctx;
    let mut m = serde_json::Map::new();
    for &which in set {
        let v = ex_try(|| match which {
            "eval" => match e.eval(c, st, row) {
                Ok(d) => json!({ "v": ex_datum(&d) }),
                Err(err) => ex_err(&err),
            },
            "int" => ex_val(e.eval_int(c, st, row), |v| json!(v.to_string())),
            "real" => ex_val(e.eval_real(c, st, row), |v| json!(v.to_bits().to_string())),
            "string" => ex_val(e.eval_string(c, st, row), |v| json!(hex(&v))),
            "decimal" => ex_val(e.eval_decimal(c, st, row), |v| dec_out(&v)),
            "time" => ex_val(e.eval_time(c, st, row), |v| time_out(&v)),
            "duration" => ex_val(e.eval_duration(c, st, row), |v| {
                json!([v.duration.to_string(), v.fsp])
            }),
            "json" => ex_val(e.eval_json(c, st, row), |v| {
                json!([v.type_code, hex(&v.value)])
            }),
            "vector" => ex_val(e.eval_vector_float32(c, st, row), |v| json!(v.string())),
            _ => unreachable!(),
        });
        m.insert(which.into(), v);
    }
    m.insert("warns".into(), warns(&ctx.truncate_warnings(0)));
    m
}

fn eval_ctx(v: &Value) -> Arc<es::EvalContext> {
    es::new_eval_context(eval_opts(v))
}

fn ft_out(ft: &FieldType) -> Value {
    json!([
        ft.get_type(),
        ft.get_flag(),
        ft.get_flen(),
        ft.get_decimal(),
        ft.get_charset(),
        ft.get_collate()
    ])
}

fn const_misc(ctx: &Arc<es::EvalContext>, c: &Constant) -> serde_json::Map<String, Value> {
    let ec: &dyn EvalContext = &**ctx;
    let e = Expr::Constant(c.clone()).into_ref();
    let mut m = serde_json::Map::new();
    m.insert("mem".into(), ex_try(|| json!(c.memory_usage())));
    m.insert("level".into(), json!(c.const_level().0));
    m.insert("corr".into(), json!(c.is_correlated()));
    m.insert("vec".into(), json!(c.vectorized()));
    m.insert("coer".into(), ex_try(|| json!(c.coercibility().0)));
    m.insert("notnull".into(), ex_try(|| json!(expr_not_null(ec, &e))));
    m.insert("binlit".into(), json!(is_binary_literal(&e)));
    m.insert(
        "type".into(),
        ex_try(|| c.get_type(ec).map_or(Value::Null, |t| ft_out(&t))),
    );
    for r in [RedactLogDisable, RedactLogEnable, RedactLogMarker] {
        m.insert(format!("s_{r}"), ex_try(|| gs(&c.string_with_ctx(ec, r))));
    }
    m.insert("hash".into(), ex_try(|| json!(hex(c.hash_code()))));
    m.insert(
        "chash".into(),
        ex_try(|| json!(hex(c.clone().canonical_hash_code()))),
    );
    m.insert(
        "eq_clone".into(),
        ex_try(|| json!(c.equal(ec, &Expr::Constant(c.clone())))),
    );
    m.insert(
        "eq_one".into(),
        ex_try(|| json!(c.equal(ec, &Expr::Constant(new_one())))),
    );
    m.insert(
        "eq_col".into(),
        ex_try(|| {
            json!(c.equal(
                ec,
                &Expr::Column(Column {
                    ret_type: c.ret_type.clone(),
                    ..Column::default()
                })
            ))
        }),
    );
    let empty = new_schema(vec![]);
    m.insert(
        "resolve".into(),
        ex_try(|| json!(matches!(c.resolve_indices(&e, &empty), Ok(r) if Arc::ptr_eq(&r, &e)))),
    );
    m.insert(
        "rvirt".into(),
        ex_try(|| {
            let (r, ok) = c.resolve_indices_by_virtual_expr(&e, ec, &empty);
            json!(Arc::ptr_eq(&r, &e) && ok)
        }),
    );
    m.insert(
        "remap".into(),
        ex_try(|| {
            json!(matches!(c.remap_column(&e, &HashMap::new()), Ok(r) if Arc::ptr_eq(&r, &e)))
        }),
    );
    m.insert(
        "decor".into(),
        json!(Arc::ptr_eq(&c.decorrelate(&e, &empty), &e)),
    );
    m
}

struct Same;

impl TraverseAction for Same {
    fn transform(&mut self, e: &ExprRef) -> ExprRef {
        Arc::clone(e)
    }
}

fn col_dump(c: &Column) -> Value {
    json!([
        c.id,
        c.unique_id,
        c.index,
        c.orig_name,
        c.is_hidden,
        ft_out(&c.ret_type)
    ])
}

/// exCols: columns from [uid, id, isPrefix, origName].
fn ex_cols(v: &Value) -> Vec<Arc<Column>> {
    v.as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            Arc::new(Column {
                unique_id: s[0].as_i64().unwrap(),
                id: s[1].as_i64().unwrap(),
                is_prefix: s[2].as_bool().unwrap(),
                orig_name: s[3].as_str().unwrap().to_string(),
                ret_type: new_field_type(mysql::TypeLonglong),
                index: i as isize,
                ..Column::default()
            })
        })
        .collect()
}

fn ex_keys(cols: &[Arc<Column>], v: &Value) -> Vec<KeyInfo> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|k| {
            k.as_array()
                .unwrap()
                .iter()
                .map(|i| Arc::clone(&cols[i.as_u64().unwrap() as usize]))
                .collect()
        })
        .collect()
}

fn uids(cols: &[Arc<Column>]) -> Value {
    json!(cols.iter().map(|c| c.unique_id).collect::<Vec<_>>())
}

fn exec(op: &str, p: &[Value]) -> Value {
    let st = EvalState::new();
    match op {
        "col" | "corr" => {
            let ft = ft_of(&p[0]);
            let ctx = eval_ctx(&p[2]);
            if op == "col" {
                let mut c = chunk::new_chunk_with_capacity(std::slice::from_ref(&ft), 1);
                append(&mut c, &ft, 0, &p[1]);
                let col = Expr::Column(Column {
                    ret_type: ft.clone(),
                    index: 0,
                    unique_id: 1,
                    ..Column::default()
                });
                return Value::Object(ex_evals(
                    &ctx,
                    &col,
                    &st,
                    c.get_row(0),
                    &ex_eval_set(&ft, 0, false),
                ));
            }
            let ec: &dyn EvalContext = &*ctx;
            let d = datum_in(&p[1]);
            let cc = CorrelatedColumn::new(Column {
                ret_type: ft.clone(),
                unique_id: 7,
                orig_name: "t.c".into(),
                ..Column::default()
            });
            let mut st = EvalState::new();
            st.set_correlated(&cc, d);
            let e = Expr::CorrelatedColumn(cc.clone()).into_ref();
            let mut m = ex_evals(&ctx, &e, &st, Row::default(), &ex_eval_set(&ft, 0, false));
            let mut sch = new_schema(vec![Arc::new(Column {
                unique_id: 8,
                ..Column::default()
            })]);
            if p[3].as_bool().unwrap() {
                sch.append([Arc::new(Column {
                    unique_id: 7,
                    ..Column::default()
                })]);
            }
            m.insert(
                "decor".into(),
                ex_try(|| json!(e.decorrelate(&sch).as_column().is_some())),
            );
            // Deviation (spec §3.2/§4): Data lives in the EvalState, so the node counts the
            // zero Datum; Go counts the current value. The record carries the value's size.
            let dmem = st.correlated(&cc).mem_usage();
            m.insert(
                "mem".into(),
                json!(cc.memory_usage() - Datum::default().mem_usage() + dmem),
            );
            m.insert("dmem".into(), json!(dmem));
            m.insert("corr".into(), json!(cc.is_correlated()));
            m.insert("level".into(), json!(cc.const_level().0));
            let other = Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
                unique_id: 8,
                ..Column::default()
            }));
            m.insert(
                "eq".into(),
                json!([
                    e.equal(ec, &Expr::CorrelatedColumn(cc.clone())),
                    e.equal(ec, &Expr::Column(cc.column.clone())),
                    cc.equal_column(&other)
                ]),
            );
            m.insert(
                "remap".into(),
                ex_try(|| {
                    let mp = HashMap::from([(
                        7,
                        Arc::new(Column {
                            unique_id: 9,
                            ret_type: ft.clone(),
                            ..Column::default()
                        }),
                    )]);
                    match e.remap_column(&mp) {
                        Err(err) => ex_err(&err),
                        Ok(r) => {
                            let nc = r.as_correlated_column().unwrap();
                            json!([nc.column.unique_id, nc.slot == cc.slot])
                        }
                    }
                }),
            );
            m.insert(
                "remap_miss".into(),
                ex_try(|| ex_err(&e.remap_column(&HashMap::new()).unwrap_err())),
            );
            m.insert(
                "resolve".into(),
                ex_try(|| {
                    let r = cc.resolve_indices(&e, &sch);
                    let (r2, ok) = cc.resolve_indices_by_virtual_expr(&e, ec, &sch);
                    let ok1 = r.is_ok();
                    json!([
                        matches!(&r, Ok(x) if Arc::ptr_eq(x, &e)),
                        ok1,
                        Arc::ptr_eq(&r2, &e),
                        ok
                    ])
                }),
            );
            m.insert(
                "trav".into(),
                json!(Arc::ptr_eq(&e.traverse(&mut Same), &e)),
            );
            Value::Object(m)
        }
        "const" => {
            let d = datum_in(&p[0]);
            let ft = ft_in(&p[1]);
            let ctx = eval_ctx(&p[2]);
            let mut c = Constant::new(d.clone(), ft.clone());
            let mut k = d.kind();
            if !p[3].is_null() {
                let order = p[3].as_i64().unwrap();
                c.param_marker = Some(ParamMarker { order });
                if let Ok(pv) = ctx.get_param_value(order) {
                    k = pv.kind();
                }
            }
            let e = Expr::Constant(c.clone()).into_ref();
            let mut m = ex_evals(&ctx, &e, &st, Row::default(), &ex_eval_set(&ft, k, true));
            m.extend(const_misc(&ctx, &c));
            m.insert(
                "trav".into(),
                json!(Arc::ptr_eq(&e.traverse(&mut Same), &e)),
            );
            Value::Object(m)
        }
        "new" => {
            let n = p[1].as_i64().unwrap();
            let c = match p[0].as_str().unwrap() {
                "one" => new_one(),
                "sone" => new_signed_one(),
                "zero" => new_zero(),
                "szero" => new_signed_zero(),
                "u64" => new_uint64_const(n as isize),
                "u64ft" => {
                    new_uint64_const_with_field_type(n as u64, new_field_type(mysql::TypeLong))
                }
                "i64" => new_int64_const(n),
                "str" => new_str_const(n.to_string().as_bytes()),
                "null" => new_null(),
                "nullft" => new_null_with_field_type(new_field_type(mysql::TypeVarchar)),
                other => panic!("new {other}"),
            };
            let ctx = es::new_eval_context(vec![]);
            json!({ "d": ex_datum(&c.value), "ft": ft_out(&c.ret_type), "misc": Value::Object(const_misc(&ctx, &c)) })
        }
        "colmisc" => {
            let mut col = Column {
                unique_id: p[0].as_i64().unwrap(),
                id: p[1].as_i64().unwrap(),
                orig_name: p[2].as_str().unwrap().into(),
                is_hidden: p[3].as_bool().unwrap(),
                ret_type: ft_of(&p[4]),
                ..Column::default()
            };
            if let Some(cs) = p[5].as_array().filter(|cs| cs.len() == 2) {
                let (cs0, cs1) = (cs[0].as_str().unwrap(), cs[1].as_str().unwrap());
                col.collation_info.set_charset_and_collation(cs0, cs1);
                col.collation_info
                    .set_repertoire(Repertoire((cs0.len() % 4) as i32));
                col.collation_info.set_explicit_charset(cs1.len() % 2 == 0);
            }
            let ctx = es::new_eval_context(vec![]);
            let ec: &dyn EvalContext = &*ctx;
            let mut m = serde_json::Map::new();
            m.insert("mem".into(), json!(col.memory_usage()));
            m.insert("str".into(), gs(&col.to_string()));
            m.insert(
                "strctx".into(),
                gs(&col.string_with_ctx(&**EmptyParamValues, RedactLogEnable)),
            );
            m.insert("hash".into(), json!(hex(col.hash_code())));
            m.insert("chash".into(), json!(hex(col.canonical_hash_code())));
            m.insert("mem2".into(), json!(col.memory_usage()));
            let mut cl = col.clone();
            cl.clean_hash_code();
            m.insert("hash_clean".into(), json!(hex(cl.hash_code())));
            m.insert("coer".into(), json!(col.coercibility().0));
            m.insert("rep".into(), json!(col.repertoire().0));
            let (cs, co) = col.collation_info.charset_and_collation();
            m.insert(
                "cc".into(),
                json!([cs, co, col.collation_info.is_explicit_charset()]),
            );
            let e = Expr::Column(col.clone()).into_ref();
            m.insert(
                "eq".into(),
                json!([
                    e.equal(ec, &Expr::Column(cl.clone())),
                    col.equal_column(&Expr::Column(Column {
                        unique_id: col.unique_id.wrapping_add(1),
                        ..Column::default()
                    })),
                    e.equal(
                        ec,
                        &Expr::CorrelatedColumn(CorrelatedColumn::new(col.clone()))
                    )
                ]),
            );
            m.insert(
                "misc".into(),
                json!([
                    col.is_correlated(),
                    col.const_level().0,
                    col.vectorized(),
                    Arc::ptr_eq(&e.decorrelate(&new_schema(vec![])), &e),
                    Arc::ptr_eq(&e.traverse(&mut Same), &e)
                ]),
            );
            let info = col.to_info();
            m.insert("info".into(), json!([info.id, ft_out(&info.field_type)]));
            let col_arc = Arc::new(col.clone());
            let cols = vec![
                Arc::new(Column {
                    id: col.id.wrapping_add(1),
                    ..Column::default()
                }),
                Arc::clone(&col_arc),
                Arc::new(Column {
                    id: col.id,
                    ..Column::default()
                }),
            ];
            m.insert("c2c".into(), ex_try(|| json!(matches!(col_info_to_col(&cols, &info), Some(r) if r.unique_id == cols[1].unique_id))));
            m.insert("c2e".into(), json!(column2_exprs(&cols).len()));
            m.insert(
                "shard".into(),
                json!([
                    gc_column_expr_is_tidb_shard(None),
                    gc_column_expr_is_tidb_shard(Some(&e))
                ]),
            );
            Value::Object(m)
        }
        "schema" => {
            let cols = ex_cols(&p[0]);
            let mut s = new_schema(cols.clone());
            s.set_keys(ex_keys(&cols, &p[1]));
            s.set_unique_keys(ex_keys(&cols, &p[2]));
            let qs = ex_cols(&p[3]);
            let mut m = serde_json::Map::new();
            m.insert("str".into(), gs(&s.to_string()));
            m.insert("len".into(), json!(s.len()));
            let mut idx = vec![];
            for q in &qs {
                let ru = s.retrieve_column(q).map(|r| r.unique_id);
                let res = ex_try(|| match q.resolve_indices(&s) {
                    Err(err) => ex_err(&err),
                    Ok(c) => json!(c.index),
                });
                let qe = Expr::Column((**q).clone());
                idx.push(json!([
                    s.column_index(q),
                    s.contains(q),
                    ru,
                    res,
                    q.in_column_array(&cols),
                    expr_reference_schema(&qe, &s),
                    expr_from_schema(&qe, &s)
                ]));
            }
            m.insert("q".into(), Value::Array(idx));
            m.insert(
                "uniq".into(),
                json!([s.is_unique(true, &qs), s.is_unique(false, &qs)]),
            );
            m.insert("ci".into(), json!(s.columns_indices(&qs)));
            let groups: Vec<Vec<Arc<Column>>> = p[4]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| {
                    g.as_array()
                        .unwrap()
                        .iter()
                        .map(|i| Arc::clone(&qs[i.as_u64().unwrap() as usize % qs.len()]))
                        .collect()
                })
                .collect();
            let (e, o) = s.extract_col_groups(&groups);
            m.insert("cg".into(), json!([e, o]));
            if let Some(ci) = s.columns_indices(&qs) {
                m.insert("cbi".into(), uids(&s.columns_by_indices(&ci)));
            }
            if let Some(h) = s.get_extra_handle_column() {
                m.insert("handle".into(), json!(h.unique_id));
            }
            let cl = s.clone_schema();
            m.insert(
                "clone".into(),
                json!([
                    gs(&cl.to_string()),
                    s.equal(&cl),
                    s.equal(&new_schema(qs.clone())),
                    cl.unique_keys.len()
                ]),
            );
            let qsch = new_schema(qs.clone());
            m.insert(
                "merge".into(),
                json!([
                    gs(&merge_schema(Some(&s), Some(&qsch)).unwrap().to_string()),
                    gs(&merge_schema(None, Some(&s)).unwrap().to_string()),
                    gs(&merge_schema(Some(&s), None).unwrap().to_string()),
                    merge_schema(None, None).is_none()
                ]),
            );
            m.insert("sorted".into(), uids(&sort_columns(&qs)));
            let one = Expr::Constant(new_one());
            m.insert(
                "konst".into(),
                json!([expr_reference_schema(&one, &s), expr_from_schema(&one, &s)]),
            );
            m.insert(
                "remap".into(),
                ex_try(|| {
                    let mp: HashMap<i64, Arc<Column>> = cols
                        .iter()
                        .map(|c| {
                            (
                                c.unique_id,
                                Arc::new(Column {
                                    unique_id: c.unique_id + 100,
                                    ..Column::default()
                                }),
                            )
                        })
                        .collect();
                    Value::Array(
                        qs.iter()
                            .map(|q| match q.remap_column(&mp) {
                                Err(err) => ex_err(&err),
                                Ok(e) => json!(e.as_column().unwrap().unique_id),
                            })
                            .collect(),
                    )
                }),
            );
            s.append(qs.iter().cloned());
            m.insert("appended".into(), json!(s.len()));
            Value::Object(m)
        }
        "tbl" => {
            let mut tbl = tidb_model::TableInfo::default();
            tbl.unmarshal_json(p[0].as_str().unwrap().as_bytes())
                .expect("bad table text");
            let bctx: BuildContextRef = es::new_expr_context(vec![es::with_column_id_allocator(
                Arc::new(new_simple_plan_column_id_allocator(p[2].as_i64().unwrap())),
            )]);
            ex_try(|| {
                let db = p[1].as_str().unwrap();
                match table_info_to_schema_and_names(
                    &bctx,
                    &CIStr {
                        o: db.into(),
                        l: db.to_lowercase(),
                    },
                    &tbl,
                ) {
                    Err(err) => ex_err(&err),
                    Ok((s, names)) => json!({
                        "str": gs(&s.to_string()),
                        "cols": s.columns.iter().map(|c| col_dump(c)).collect::<Vec<_>>(),
                        "names": names.iter().map(|n| gs(n.string().as_bytes())).collect::<Vec<_>>(),
                        "keys": s.keys.iter().map(|k| uids(k)).collect::<Vec<_>>(),
                    }),
                }
            })
        }
        "evalbool" => {
            let ctx = eval_ctx(&p[1]);
            let ec: &dyn EvalContext = &*ctx;
            let exprs: Vec<ExprRef> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    Expr::Constant(Constant::new(
                        datum_in(d),
                        new_field_type(mysql::TypeVarchar),
                    ))
                    .into_ref()
                })
                .collect();
            ex_try(|| {
                let r = match eval_bool(ec, &st, &exprs, Row::default()) {
                    Err(err) => ex_err(&err),
                    Ok((b, n)) => json!({ "v": [b, n] }),
                };
                json!({ "r": r, "warns": warns(&ctx.truncate_warnings(0)), "eqin": is_eq_cond_from_in(&exprs[0]) })
            })
        }
        other => panic!("unknown op {other}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "expr");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap();
        let got = if matches!(op, "sf" | "sfpair" | "vcol" | "sizes") {
            exec_sf(op, &args, &rec["out"])
        } else {
            exec(op, &args)
        };
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
fn govec_expr() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_EXPR_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

#[allow(dead_code)]
fn _uses(_: &dyn WarnHandler) {}

// ---------------------------------------------------------------- part 2: ScalarFunction

/// Descriptor-only signatures for the trees Go built: the name of Go's signature type and
/// its own vectorized() as Go showed it; no evaluation (the function families come later).
fn sig_of(name: &str, vectorized: bool) -> &'static eval::SigDesc {
    use std::sync::{Mutex, OnceLock};
    static SIGS: OnceLock<Mutex<HashMap<(String, bool), &'static eval::SigDesc>>> = OnceLock::new();
    let mut m = SIGS.get_or_init(Default::default).lock().unwrap();
    m.entry((name.to_owned(), vectorized)).or_insert_with(|| {
        Box::leak(Box::new(eval::SigDesc {
            name: Box::leak(name.to_owned().into_boxed_str()),
            pb_code: 0,
            vectorized,
            drivers: eval::SigDrivers::None,
        }))
    })
}

fn sf_tree(v: &Value) -> ExprRef {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        let a = a.as_array().unwrap();
        return Expr::Column(Column {
            unique_id: a[0].as_i64().unwrap(),
            index: a[1].as_i64().unwrap() as isize,
            ret_type: ft_in(&a[2]),
            orig_name: String::from_utf8(ungs(&a[3])).unwrap(),
            virtual_expr: a.get(4).filter(|x| !x.is_null()).map(sf_tree),
            ..Column::default()
        })
        .into_ref();
    }
    if let Some(a) = o.get("corr") {
        let a = a.as_array().unwrap();
        return Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
            unique_id: a[0].as_i64().unwrap(),
            ret_type: ft_in(&a[1]),
            ..Column::default()
        }))
        .into_ref();
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1]))).into_ref();
    }
    let a = o["f"].as_array().unwrap();
    let args: Vec<ExprRef> = a[4].as_array().unwrap().iter().map(sf_tree).collect();
    let ret = ft_in(&a[3]);
    let sig = sig_of(a[2].as_str().unwrap(), a[5].as_bool().unwrap());
    let mut f = eval::BuiltinFunc::new(sig, args, ret.clone(), a[6].as_str().unwrap(), a[7].as_str().unwrap());
    f.pb_code = a[8].as_i64().unwrap() as i32;
    if a[9].as_bool().unwrap() {
        f.collation_info.set_coercibility(Coercibility(a[10].as_i64().unwrap() as _));
    }
    f.collation_info.set_repertoire(Repertoire(a[11].as_i64().unwrap() as _));
    f.collation_info.set_explicit_charset(a[12].as_bool().unwrap());
    let mut sf = ScalarFunction::new(a[1].as_str().unwrap(), ret, f);
    sf.func_name = CIStr::new(a[1].as_str().unwrap());
    Expr::ScalarFunction(sf).into_ref()
}

fn sf_leaf_idx(e: &Expr) -> Value {
    let mut out = vec![];
    fn walk(e: &Expr, out: &mut Vec<Value>) {
        match e {
            Expr::Column(c) => out.push(json!([c.unique_id, c.index])),
            Expr::ScalarFunction(f) => f.get_args().iter().for_each(|a| walk(a, out)),
            _ => {}
        }
    }
    walk(e, &mut out);
    json!(out)
}

fn sf_str(e: &Expr) -> Value {
    gs(&e.string_with_ctx(&**EmptyParamValues, RedactLogDisable))
}

fn exec_sf(op: &str, p: &[Value], want: &Value) -> Value {
    let ctx = es::new_eval_context(vec![]);
    let ec: &dyn EvalContext = &*ctx;
    match op {
        "sizes" => json!({ "sf": 104, "base": 152, "once": 12 }),
        "sf" => {
            if want.get("build").is_some() || want.get("notsf").is_some() {
                return want.clone(); // Go could not build the recipe: nothing to compare.
            }
            let e = sf_tree(&want["tree"]);
            let sf = e.as_scalar_function().unwrap();
            let mut m = serde_json::Map::new();
            m.insert("mem".into(), json!(sf.memory_usage()));
            m.insert("tree".into(), want["tree"].clone());
            m.insert("str".into(), sf_str(&e));
            m.insert("hash".into(), json!(hex(sf.hash_code())));
            m.insert("canon".into(), json!(hex(sf.canonical_hash_code())));
            m.insert("level".into(), json!(sf.const_level().0));
            m.insert("corr".into(), json!(sf.is_correlated()));
            m.insert("vec".into(), json!(sf.vectorized()));
            let single: Vec<Value> = [false, true]
                .iter()
                .map(|&rev| {
                    let (c, r) = sf.get_single_column(rev);
                    json!([c.map(|c| c.unique_id), r])
                })
                .collect();
            m.insert("single".into(), json!(single));
            let sch = new_schema(
                p[1].as_array().unwrap().iter().map(|u| Arc::new(Column { unique_id: u.as_i64().unwrap(), ..Column::default() })).collect(),
            );
            m.insert(
                "resolve".into(),
                ex_try(|| match e.resolve_indices(&sch) {
                    Err(err) => ex_err(&err),
                    Ok(r) => json!([sf_leaf_idx(&r), hex(r.hash_code())]),
                }),
            );
            m.insert(
                "vresolve".into(),
                ex_try(|| {
                    let (r, ok) = e.resolve_indices_by_virtual_expr(ec, &sch);
                    json!([sf_leaf_idx(&r), ok])
                }),
            );
            let remap: HashMap<i64, Arc<Column>> = p[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|pr| {
                    let pr = pr.as_array().unwrap();
                    (
                        pr[0].as_i64().unwrap(),
                        Arc::new(Column { unique_id: pr[1].as_i64().unwrap(), ret_type: new_field_type(mysql::TypeLonglong), ..Column::default() }),
                    )
                })
                .collect();
            m.insert(
                "remap".into(),
                ex_try(|| match e.remap_column(&remap) {
                    Err(err) => ex_err(&err),
                    Ok(r) => json!([sf_str(&r), hex(r.hash_code())]),
                }),
            );
            let cl = Expr::ScalarFunction(sf.clone()).into_ref();
            m.insert("clone".into(), json!([e.equal(ec, &cl), sf_str(&cl), hex(cl.hash_code())]));
            m.insert(
                "decor".into(),
                ex_try(|| {
                    let d = cl.decorrelate(&sch);
                    json!([sf_str(&d), d.is_correlated(), hex(d.hash_code())])
                }),
            );
            struct Nop;
            impl TraverseAction for Nop {
                fn transform(&mut self, e: &ExprRef) -> ExprRef {
                    Arc::clone(e)
                }
            }
            m.insert("trav".into(), json!(Arc::ptr_eq(&e.traverse(&mut Nop), &e)));
            Value::Object(m)
        }
        "sfpair" => {
            if want.get("build").is_some() {
                return want.clone();
            }
            let (a, b) = (sf_tree(&want["a"]), sf_tree(&want["b"]));
            json!({ "a": want["a"], "b": want["b"], "eq": a.equal(ec, &b), "seq": expressions_semantic_equal(&a, &b) })
        }
        "vcol" => {
            if want.get("build").is_some() {
                return want.clone();
            }
            let cols: Vec<Arc<Column>> = want["cols"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| Arc::new(sf_tree(c).as_column().unwrap().clone()))
                .collect();
            let sch = new_schema(cols.clone());
            let used: Vec<Arc<Column>> = p[1].as_array().unwrap().iter().map(|u| Arc::clone(&cols[u.as_u64().unwrap() as usize])).collect();
            let probe = sf_tree(&want["probe"]).as_column().unwrap().clone();
            let eqid: Vec<bool> = cols.iter().map(|c| probe.equal_by_expr_and_id(ec, c)).collect();
            let (r, ok) = probe.resolve_indices_by_virtual_expr(ec, &sch);
            json!({ "cols": want["cols"], "probe": want["probe"], "used": get_used_list(ec, &used, &sch), "eqid": eqid, "vres": [r.index, ok] })
        }
        _ => unreachable!(),
    }
}
