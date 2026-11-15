//! The comparison ops of the govec `cast` domain (tidb-rs-tracker
//! `govec/domains/cast_d084.go`, D08.4 batch 1): "cmpbuild" builds `name(lhs, rhs)` through
//! `compareFunctionClass` and evaluates it with vectorization off and on; it also records
//! `GetAccurateCmpType` and the `CompareFunc` of `GetCmpFunction` on every row.

use serde_json::{Map, Value, json};
use tidb_base::contextutil::WarnHandler;
use tidb_chunk::Row;
use tidb_expression::builtin_compare::{
    get_accurate_cmp_type, get_cmp_function, resolve_type4_between,
};
use tidb_expression::eval::{RpnState, rt};
use tidb_expression::expression::eval_expr;
use tidb_expression::scalar_function::{new_function, new_function_base};
use tidb_expression::*;
use tidb_parser::types::{ETDatetime, ETInt, ETTimestamp};
use tidb_types::field_type::new_field_type;

use super::cast_eval::{column, err_or_null, expr_of, input_of};
use super::{ctx_of, dump, ex_err, ex_try, warns};

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    if op == "between" {
        return Some(between(p));
    }
    if op != "cmpbuild" {
        return None;
    }
    let st = EvalState::new();
    let ctx = ctx_of(&p[4]);
    let ev = &ctx.1;
    let (fts, input) = input_of(&p[5], &p[6], &p[7]);
    let mut m = Map::new();
    let mut lr: Option<(ExprRef, ExprRef)> = None;
    m.insert(
        "acc".into(),
        ex_try(|| {
            let l = expr_of(&ctx, &p[2], &fts);
            let r = expr_of(&ctx, &p[3], &fts);
            ev.truncate_warnings(0);
            let acc = get_accurate_cmp_type(&**ev, &l, &r);
            lr = Some((l, r));
            json!(acc.0)
        }),
    );
    let Some((l, r)) = lr else {
        return Some(Value::Object(m));
    };
    m.insert(
        "cf".into(),
        ex_try(|| {
            let acc = get_accurate_cmp_type(&**ev, &l, &r);
            let same = |e: &Expr| {
                let et = e.get_type(&**ev).eval_type();
                if acc == ETDatetime || acc == ETTimestamp {
                    et == ETDatetime || et == ETTimestamp
                } else {
                    et == acc
                }
            };
            if !same(&l) || !same(&r) {
                return Value::Null;
            }
            let f = get_cmp_function(&*ctx.0, &l, &r);
            let mut out = Vec::new();
            for i in 0..input.num_rows() {
                let row: Row<'_> = input.get_row(i);
                out.push(match f.call(&**ev, &st, &l, &r, row, row) {
                    Err(e) => ex_err(&e),
                    Ok((v, is_null)) => json!([v, is_null]),
                });
            }
            Value::Array(out)
        }),
    );
    m.insert("cfwarns".into(), warns(&ev.truncate_warnings(0)));
    let mut built: Option<ExprRef> = None;
    m.insert(
        "res".into(),
        ex_try(|| {
            let tp = new_field_type(tidb_base::mysql::TypeUnspecified);
            let name = p[1].as_str().unwrap();
            let args = vec![std::sync::Arc::clone(&l), std::sync::Arc::clone(&r)];
            let res = if p[0].as_i64().unwrap() == 0 {
                new_function_base(&*ctx.0, name, tp, args)
            } else {
                new_function(&*ctx.0, name, tp, args)
            };
            match res {
                Err(e) => ex_err(&e),
                Ok(e) => {
                    let d = dump(&e);
                    built = Some(e);
                    d
                }
            }
        }),
    );
    m.insert("bwarns".into(), warns(&ev.truncate_warnings(0)));
    if let Some(e) = built {
        for (key, on) in [("row", false), ("vec", true)] {
            m.insert(
                key.into(),
                ex_try(|| {
                    let mut rpn = RpnState::default();
                    let mut col = rt::empty_column();
                    let r = eval_expr(&**ev, &st, &mut rpn, on, &e, ETInt, &input, &mut col);
                    let mut o = Map::new();
                    o.insert("err".into(), err_or_null(&r));
                    o.insert("warns".into(), warns(&ev.truncate_warnings(0)));
                    if r.is_ok() {
                        o.insert(
                            "v".into(),
                            column(
                                &col,
                                &new_field_type(tidb_base::mysql::TypeLonglong),
                                col.rows(),
                            ),
                        );
                    }
                    Value::Object(o)
                }),
            );
        }
    }
    Some(Value::Object(m))
}

/// "between" (D08.4 batch 3, `cast_zd084c.go`): `ResolveType4Between(a, b, c)`.
fn between(p: &[Value]) -> Value {
    let ctx = ctx_of(&p[0]);
    let ev = &ctx.1;
    let fts: Vec<_> = p[1].as_array().unwrap().iter().map(super::ft_in).collect();
    json!({"et": ex_try(|| {
        let es: Vec<ExprRef> =
            p[2].as_array().unwrap().iter().map(|e| expr_of(&ctx, e, &fts)).collect();
        ev.truncate_warnings(0);
        json!(resolve_type4_between(&**ev, [&es[0], &es[1], &es[2]]).0)
    })})
}
