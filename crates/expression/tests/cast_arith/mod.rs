//! The arithmetic ops of the govec `cast` domain (tidb-rs-tracker
//! `govec/domains/cast_d085.go`, D08.5 batch 1): "arbuild" builds `name(args..)` (the
//! arithmetic operators, CEIL / CEILING / FLOOR) with NewFunctionBase, NewFunction or
//! NewFunctionTryFold and evaluates it with vectorization off and on, as its own
//! evaluation type.

use serde_json::{Map, Value};
use tidb_base::contextutil::WarnHandler;
use tidb_expression::eval::{RpnState, rt};
use tidb_expression::expression::eval_expr;
use tidb_expression::scalar_function::{new_function, new_function_base, new_function_try_fold};
use tidb_expression::*;
use tidb_types::field_type::new_field_type;

use super::cast_eval::{column, err_or_null, expr_of, input_of};
use super::{ctx_of, dump, ex_err, ex_try, warns};

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    if op != "arbuild" {
        return None;
    }
    let st = EvalState::new();
    let ctx = ctx_of(&p[3]);
    let ev = &ctx.1;
    let (fts, input) = input_of(&p[4], &p[5], &p[6]);
    let mut m = Map::new();
    let mut args: Option<Vec<ExprRef>> = None;
    m.insert(
        "args".into(),
        ex_try(|| {
            let a: Vec<ExprRef> = p[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| expr_of(&ctx, r, &fts))
                .collect();
            ev.truncate_warnings(0);
            args = Some(a);
            Value::Null
        }),
    );
    let Some(args) = args else {
        return Some(Value::Object(m));
    };
    let mut built: Option<ExprRef> = None;
    m.insert(
        "res".into(),
        ex_try(|| {
            let tp = new_field_type(tidb_base::mysql::TypeUnspecified);
            let name = p[1].as_str().unwrap();
            let res = match p[0].as_i64().unwrap() {
                0 => new_function_base(&*ctx.0, name, tp, args.clone()),
                1 => new_function(&*ctx.0, name, tp, args.clone()),
                _ => new_function_try_fold(&*ctx.0, name, tp, args.clone()),
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
        let tp = e.get_type(&**ev).into_owned();
        let et = tp.eval_type();
        for (key, on) in [("row", false), ("vec", true)] {
            m.insert(
                key.into(),
                ex_try(|| {
                    let mut rpn = RpnState::default();
                    let mut col = rt::empty_column();
                    let r = eval_expr(&**ev, &st, &mut rpn, on, &e, et, &input, &mut col);
                    let mut o = Map::new();
                    o.insert("err".into(), err_or_null(&r));
                    o.insert("warns".into(), warns(&ev.truncate_warnings(0)));
                    if r.is_ok() {
                        o.insert("v".into(), column(&col, &tp, col.rows()));
                    }
                    Value::Object(o)
                }),
            );
        }
    }
    Some(Value::Object(m))
}
