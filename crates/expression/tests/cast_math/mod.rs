//! The math ops of the govec `cast` domain (tidb-rs-tracker `govec/domains/cast_d085d.go`,
//! D08.5 batch 4): "mathbuild" is batch 1's "arbuild" under its own op name (its platform
//! differences are registered apart); "mathclone" builds a function, clones it and evaluates
//! the original and the clone one after the other (RAND's MysqlRng is shared by the clone,
//! D08.1 T6c (b)); "gomath" calls `tidb_base::gostd::math` on float64 bit patterns.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tidb_base::contextutil::WarnHandler;
use tidb_base::gostd::math as gm;
use tidb_expression::eval::{RpnState, rt};
use tidb_expression::expression::eval_expr;
use tidb_expression::scalar_function::{new_function, new_function_base};
use tidb_expression::*;
use tidb_types::field_type::new_field_type;

use super::cast_eval::{column, err_or_null, expr_of, input_of};
use super::{ctx_of, dump, ex_err, ex_try, warns};

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    match op {
        "mathbuild" => super::cast_arith::exec("arbuild", p),
        // D08.5 batch 5 (CONV, CRC32, `govec/domains/cast_d085e.go`): batch 1's "arbuild".
        "convbuild" => super::cast_arith::exec("arbuild", p),
        "mathclone" => Some(clone(p)),
        "gomath" => Some(go_math(p)),
        _ => None,
    }
}

fn clone(p: &[Value]) -> Value {
    let st = EvalState::new();
    let ctx = ctx_of(&p[3]);
    let ev = &ctx.1;
    let (fts, input) = input_of(&p[4], &p[5], &p[6]);
    let mut m = Map::new();
    let mut built: Option<(ExprRef, ExprRef)> = None;
    m.insert(
        "res".into(),
        ex_try(|| {
            let args: Vec<ExprRef> = p[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| expr_of(&ctx, r, &fts))
                .collect();
            ev.truncate_warnings(0);
            let tp = new_field_type(tidb_base::mysql::TypeUnspecified);
            let name = p[1].as_str().unwrap();
            let res = if p[0].as_i64().unwrap() == 0 {
                new_function_base(&*ctx.0, name, tp, args)
            } else {
                new_function(&*ctx.0, name, tp, args)
            };
            match res {
                Err(e) => ex_err(&e),
                Ok(e) => {
                    // go: Expression.Clone
                    let c: ExprRef = Arc::new((*e).clone());
                    let d = dump(&c);
                    built = Some((e, c));
                    d
                }
            }
        }),
    );
    ev.truncate_warnings(0);
    let Some((e, c)) = built else {
        return Value::Object(m);
    };
    let tp = e.get_type(&**ev).into_owned();
    let et = tp.eval_type();
    let mut out = vec![];
    for s in p[7].as_array().unwrap() {
        let s = s.as_i64().unwrap();
        let x = if s & 1 != 0 { &c } else { &e };
        out.push(ex_try(|| {
            let mut rpn = RpnState::default();
            let mut col = rt::empty_column();
            let r = eval_expr(&**ev, &st, &mut rpn, s & 2 != 0, x, et, &input, &mut col);
            let mut o = Map::new();
            o.insert("err".into(), err_or_null(&r));
            o.insert("warns".into(), warns(&ev.truncate_warnings(0)));
            if r.is_ok() {
                o.insert("v".into(), column(&col, &tp, col.rows()));
            }
            Value::Object(o)
        }));
    }
    m.insert("steps".into(), Value::Array(out));
    Value::Object(m)
}

fn go_math(p: &[Value]) -> Value {
    let x = f64::from_bits(p[1].as_u64().unwrap());
    let yb = p[2].as_u64().unwrap();
    let y = f64::from_bits(yb);
    let bits = |f: f64| json!(f.to_bits().to_string());
    match p[0].as_str().unwrap() {
        "exp" => bits(gm::exp(x)),
        "log" => bits(gm::log(x)),
        "log2" => bits(gm::log2(x)),
        "log10" => bits(gm::log10(x)),
        "pow" => bits(gm::pow(x, y)),
        "sin" => bits(gm::sin(x)),
        "cos" => bits(gm::cos(x)),
        "tan" => bits(gm::tan(x)),
        "asin" => bits(gm::asin(x)),
        "acos" => bits(gm::acos(x)),
        "atan" => bits(gm::atan(x)),
        "atan2" => bits(gm::atan2(x, y)),
        // go: math.Sqrt is an intrinsic (FSQRTD / SQRTSD), correctly rounded like f64::sqrt.
        "sqrt" => bits(x.sqrt()),
        "frexp" => {
            let (f, e) = gm::frexp(x);
            json!([f.to_bits().to_string(), e])
        }
        "ldexp" => bits(gm::ldexp(x, yb as i64)),
        f => panic!("gomath {f}"),
    }
}
