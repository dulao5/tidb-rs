//! The `ev*` ops of the govec `cast` domain (tidb-rs-tracker `govec/domains/cast2b3c.go`,
//! part 2b-3c = D08.1 part 2b): `EvaluatorSuite`, `VectorizedFilter[ConsiderNull]`,
//! `VecEvalBool`, `EvalExpr` and `VectorizedExecute` over a chunk.

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::WarnHandler;
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_expression::builtin_cast::build_cast_function_with_check;
use tidb_expression::chunk_executor::{
    vectorized_execute, vectorized_filter, vectorized_filter_consider_null,
};
use tidb_expression::eval::{RpnState, rt};
use tidb_expression::evaluator::new_evaluator_suite;
use tidb_expression::expression::{eval_expr, vec_eval_bool};
use tidb_expression::*;
use tidb_parser::types::{
    ETDatetime, ETDecimal, ETDuration, ETInt, ETJson, ETReal, ETString, ETTimestamp,
    ETVectorFloat32, EvalType, FieldType,
};
use tidb_sessionctx::exprstatic as es;
use tidb_types::field_type::new_field_type;

use super::{Ctxs, ctx_of, datum_in, dec_out, ex_try, ft_in, gs, hex, json_out, time_out, warns};

/// evInput.
pub(super) fn input_of(cols: &Value, rows: &Value, sel: &Value) -> (Vec<FieldType>, Chunk) {
    let fts: Vec<FieldType> = cols.as_array().unwrap().iter().map(ft_in).collect();
    let rows = rows.as_array().unwrap();
    let mut chk = new_chunk_with_capacity(&fts, rows.len());
    for r in rows {
        for (c, d) in r.as_array().unwrap().iter().enumerate() {
            chk.append_datum(c, &datum_in(d));
        }
    }
    if let Some(s) = sel.as_array() {
        chk.set_sel(Some(
            s.iter().map(|v| v.as_u64().unwrap() as usize).collect(),
        ));
    }
    (fts, chk)
}

/// evExpr.
pub(super) fn expr_of(ctx: &Ctxs, v: &Value, fts: &[FieldType]) -> ExprRef {
    let a = v.as_array().unwrap();
    match a[0].as_str().unwrap() {
        "col" => {
            let i = a[1].as_u64().unwrap() as usize;
            Expr::Column(Column {
                unique_id: i as i64 + 1,
                index: i as isize,
                ret_type: fts[i].clone(),
                orig_name: "c".into(),
                ..Column::default()
            })
            .into_ref()
        }
        "const" => Expr::Constant(Constant::new(datum_in(&a[1]), ft_in(&a[2]))).into_ref(),
        "cast" => build_cast_function_with_check(
            &*ctx.0,
            expr_of(ctx, &a[2], fts),
            ft_in(&a[1]),
            false,
            false,
        )
        .unwrap(),
        // D08.12: ["corcol", i] -> a correlated column over column i (UniqueID 101+i).
        "corcol" => {
            let i = a[1].as_u64().unwrap() as usize;
            Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
                unique_id: 101 + i as i64,
                index: i as isize,
                ret_type: fts[i].clone(),
                orig_name: "c".into(),
                ..Column::default()
            }))
            .into_ref()
        }
        // D08.12: ["vcol", i, recipe] -> a virtual (generated) column of column i's type
        // (UniqueID 201+i).
        "vcol" => {
            let i = a[1].as_u64().unwrap() as usize;
            Expr::Column(Column {
                unique_id: 201 + i as i64,
                index: i as isize,
                ret_type: fts[i].clone(),
                orig_name: "c".into(),
                virtual_expr: Some(expr_of(ctx, &a[2], fts)),
                ..Column::default()
            })
            .into_ref()
        }
        // D08.2 part 2 ("evnull"): ["defer", recipe(, stale datum)] -> a constant whose
        // value is the recipe's (evaluated now, or the stale datum) and whose DeferredExpr is
        // the recipe.
        "defer" => {
            let e = expr_of(ctx, &a[1], fts);
            let ev = ctx.0.get_eval_ctx();
            let mut v = e
                .eval(
                    &*ev,
                    &tidb_expression::expression::EvalState::new(),
                    Default::default(),
                )
                .unwrap();
            if a.len() > 2 {
                v = datum_in(&a[2]);
            }
            let mut c = Constant::new(v, e.get_type(&*ev).into_owned());
            c.deferred_expr = Some(e);
            Expr::Constant(c).into_ref()
        }
        // D08.4 batch 1: ["fn", name, [recipes]] -> NewFunctionBase (no folding).
        "fn" => tidb_expression::scalar_function::new_function_base(
            &*ctx.0,
            a[1].as_str().unwrap(),
            new_field_type(mysql::TypeUnspecified),
            exprs_of(ctx, &a[2], fts),
        )
        .unwrap(),
        k => panic!("expr_of: {k}"),
    }
}

fn exprs_of(ctx: &Ctxs, v: &Value, fts: &[FieldType]) -> Vec<ExprRef> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| expr_of(ctx, r, fts))
        .collect()
}

/// evCell.
fn cell(col: &tidb_chunk::Column, ft: &FieldType, i: usize) -> Value {
    if col.is_null(i) {
        return Value::Null;
    }
    match ft.get_type() {
        mysql::TypeBit => return json!(hex(col.get_bytes(i))),
        mysql::TypeEnum => {
            let e = col.get_enum(i);
            return json!([e.value, gs(&e.name)]);
        }
        mysql::TypeSet => {
            let s = col.get_set(i);
            return json!([s.value, gs(&s.name)]);
        }
        mysql::TypeFloat => return json!(col.get_float32(i).to_bits().to_string()),
        _ => {}
    }
    let et = ft.eval_type();
    if et == ETInt {
        if mysql::has_unsigned_flag(ft.get_flag()) {
            json!(col.get_uint64(i).to_string())
        } else {
            json!(col.get_int64(i).to_string())
        }
    } else if et == ETReal {
        json!(col.get_float64(i).to_bits().to_string())
    } else if et == ETDecimal {
        dec_out(&col.get_decimal(i))
    } else if et == ETString {
        gs(col.get_string(i))
    } else if et == ETDatetime || et == ETTimestamp {
        time_out(&col.get_time(i))
    } else if et == ETDuration {
        json!(col.get_duration(i, 0).duration)
    } else if et == ETJson {
        json_out(&col.get_json(i))
    } else if et == ETVectorFloat32 {
        json!(hex(col.get_vector_float32(i).zero_copy_serialize()))
    } else {
        panic!("cell")
    }
}

pub(super) fn column(col: &tidb_chunk::Column, ft: &FieldType, n: usize) -> Value {
    Value::Array((0..n).map(|i| cell(col, ft, i)).collect())
}

pub(super) fn err_or_null(r: &Result<(), Error>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => gs(&e.error()),
    }
}

/// evFtOf.
fn ft_of(et: EvalType) -> FieldType {
    new_field_type(if et == ETInt {
        mysql::TypeLonglong
    } else if et == ETReal {
        mysql::TypeDouble
    } else if et == ETDecimal {
        mysql::TypeNewDecimal
    } else if et == ETDatetime || et == ETTimestamp {
        mysql::TypeDatetime
    } else if et == ETDuration {
        mysql::TypeDuration
    } else if et == ETJson {
        mysql::TypeJSON
    } else if et == ETVectorFloat32 {
        mysql::TypeTiDBVectorFloat32
    } else {
        mysql::TypeVarString
    })
}

/// evWarns: `warns` of the context's warnings; `pooled` records the argument of a
/// "Truncated incorrect DOUBLE value" warning as '<pooled>' (Go formats it lazily from a
/// pooled column's bytes, which are reused afterwards; tidb-rs copies the string).
fn ev_warns(ev: &es::EvalContext, pooled: bool) -> Value {
    let ws = ev.truncate_warnings(0);
    Value::Array(
        ws.iter()
            .map(|w| {
                let mut msg = w.err.error();
                let p: &[u8] = b"Truncated incorrect DOUBLE value: ";
                if let Some(i) = msg.windows(p.len()).position(|x| x == p).filter(|_| pooled) {
                    msg.truncate(i + p.len());
                    msg.extend_from_slice(b"'<pooled>'");
                }
                json!([&*w.level, gs(&msg)])
            })
            .collect(),
    )
}

fn sel_of(chk: &Chunk) -> Value {
    match chk.sel() {
        None => Value::Null,
        Some(s) => json!(s),
    }
}

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    let st = EvalState::new();
    let mut rpn = RpnState::default();
    let out = match op {
        "evsuite" => {
            let ctx = ctx_of(&p[3]);
            let ev = &ctx.1;
            let (fts, mut input) = input_of(&p[4], &p[5], &p[6]);
            ex_try(|| {
                let exprs = exprs_of(&ctx, &p[0], &fts);
                ev.truncate_warnings(0);
                let out_fts: Vec<FieldType> = exprs
                    .iter()
                    .map(|e| e.get_type(&**ev).into_owned())
                    .collect();
                let s = new_evaluator_suite(&exprs, p[1].as_bool().unwrap());
                let props = s.required_optional_eval_props().0;
                let mut output = new_chunk_with_capacity(&out_fts, input.num_rows());
                let r = s.run(
                    &**ev,
                    &st,
                    &mut rpn,
                    p[2].as_bool().unwrap(),
                    &mut input,
                    &mut output,
                );
                let mut m = serde_json::Map::new();
                m.insert("vectorizable".into(), json!(s.vectorizable()));
                m.insert("props".into(), json!(props));
                m.insert("err".into(), err_or_null(&r));
                m.insert("warns".into(), warns(&ev.truncate_warnings(0)));
                if r.is_ok() {
                    let cols: Vec<Value> = out_fts
                        .iter()
                        .enumerate()
                        .map(|(i, ft)| {
                            let c = output.column(i);
                            column(c, ft, c.rows())
                        })
                        .collect();
                    m.insert("out".into(), Value::Array(cols));
                    let lens: Vec<usize> = (0..input.num_cols())
                        .map(|i| input.column(i).rows())
                        .collect();
                    m.insert("inlens".into(), json!(lens));
                }
                Value::Object(m)
            })
        }
        "evfilter" => {
            let ctx = ctx_of(&p[3]);
            let ev = &ctx.1;
            let (fts, mut input) = input_of(&p[4], &p[5], &p[6]);
            ex_try(|| {
                let filters = exprs_of(&ctx, &p[0], &fts);
                ev.truncate_warnings(0);
                let vec_enabled = p[1].as_bool().unwrap();
                let r = if p[2].as_bool().unwrap() {
                    vectorized_filter_consider_null(
                        &**ev,
                        &st,
                        &mut rpn,
                        vec_enabled,
                        &filters,
                        &mut input,
                        vec![],
                        Some(vec![]),
                    )
                } else {
                    vectorized_filter(
                        &**ev,
                        &st,
                        &mut rpn,
                        vec_enabled,
                        &filters,
                        &mut input,
                        vec![],
                    )
                    .map(|s| (s, None))
                };
                let mut m = serde_json::Map::new();
                m.insert(
                    "err".into(),
                    err_or_null(&r.as_ref().map(|_| ()).map_err(Clone::clone)),
                );
                let vec_path = vec_enabled && filters.iter().all(|f| f.vectorized());
                m.insert("warns".into(), ev_warns(ev, vec_path));
                if let Ok((selected, is_null)) = r {
                    m.insert("selected".into(), json!(selected));
                    m.insert("isnull".into(), json!(is_null));
                }
                m.insert("sel".into(), sel_of(&input));
                Value::Object(m)
            })
        }
        "evbool" => {
            let ctx = ctx_of(&p[2]);
            let ev = &ctx.1;
            let (fts, mut input) = input_of(&p[3], &p[4], &p[5]);
            ex_try(|| {
                let exprs = exprs_of(&ctx, &p[0], &fts);
                ev.truncate_warnings(0);
                let r = vec_eval_bool(
                    &**ev,
                    &st,
                    &mut rpn,
                    p[1].as_bool().unwrap(),
                    &exprs,
                    &mut input,
                    vec![],
                    vec![],
                );
                let mut m = serde_json::Map::new();
                m.insert(
                    "err".into(),
                    err_or_null(&r.as_ref().map(|_| ()).map_err(Clone::clone)),
                );
                m.insert("warns".into(), ev_warns(ev, true));
                m.insert("sel".into(), sel_of(&input));
                if let Ok((selected, nulls)) = r {
                    m.insert("selected".into(), json!(selected));
                    m.insert("nulls".into(), json!(nulls));
                }
                Value::Object(m)
            })
        }
        "evexpr" => {
            let ctx = ctx_of(&p[3]);
            let ev = &ctx.1;
            let (fts, input) = input_of(&p[4], &p[5], &p[6]);
            ex_try(|| {
                let e = expr_of(&ctx, &p[0], &fts);
                ev.truncate_warnings(0);
                let et = EvalType(p[2].as_u64().unwrap() as u8);
                let mut col = rt::empty_column();
                let r = eval_expr(
                    &**ev,
                    &st,
                    &mut rpn,
                    p[1].as_bool().unwrap(),
                    &e,
                    et,
                    &input,
                    &mut col,
                );
                let mut m = serde_json::Map::new();
                m.insert("err".into(), err_or_null(&r));
                m.insert("warns".into(), warns(&ev.truncate_warnings(0)));
                if r.is_ok() {
                    m.insert("v".into(), column(&col, &ft_of(et), col.rows()));
                }
                Value::Object(m)
            })
        }
        "evexec" => {
            let ctx = ctx_of(&p[1]);
            let ev = &ctx.1;
            let (fts, input) = input_of(&p[2], &p[3], &p[4]);
            ex_try(|| {
                let exprs = exprs_of(&ctx, &p[0], &fts);
                ev.truncate_warnings(0);
                let out_fts: Vec<FieldType> = exprs
                    .iter()
                    .map(|e| e.get_type(&**ev).into_owned())
                    .collect();
                let mut output = new_chunk_with_capacity(&out_fts, input.num_rows());
                let r = vectorized_execute(&**ev, &st, &exprs, &input, &mut output);
                let mut m = serde_json::Map::new();
                m.insert("err".into(), err_or_null(&r));
                m.insert("warns".into(), warns(&ev.truncate_warnings(0)));
                let cols: Vec<Value> = out_fts
                    .iter()
                    .enumerate()
                    .map(|(i, ft)| {
                        let c = output.column(i);
                        column(c, ft, c.rows())
                    })
                    .collect();
                m.insert("out".into(), Value::Array(cols));
                Value::Object(m)
            })
        }
        _ => return None,
    };
    Some(json!({ "res": out }))
}
