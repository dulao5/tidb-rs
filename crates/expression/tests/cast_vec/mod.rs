//! The VECTOR function ops of the govec `cast` domain (tidb-rs-tracker
//! `govec/domains/cast_d081c.go`, D08.1 part 3): "vecbuild" is D08.5 batch 1's "arbuild"
//! under its own op name; "vsh" runs `extract_vector_helper` over expression recipes.

use serde_json::{Map, Value, json};
use tidb_base::contextutil::WarnHandler;
use tidb_expression::builtin_vec::extract_vector_helper;
use tidb_expression::*;

use super::cast_eval::expr_of;
use super::{ctx_of, dump, ex_try, ft_in, gs};

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    match op {
        "vecbuild" => super::cast_arith::exec("arbuild", p),
        "vsh" => Some(vsh(p)),
        _ => None,
    }
}

fn vsh(p: &[Value]) -> Value {
    let ctx = ctx_of(&p[1]);
    let ev = &ctx.1;
    let fts: Vec<_> = p[2].as_array().unwrap().iter().map(ft_in).collect();
    let mut m = Map::new();
    let mut built: Option<Vec<ExprRef>> = None;
    m.insert(
        "args".into(),
        ex_try(|| {
            let es: Vec<ExprRef> = p[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| expr_of(&ctx, r, &fts))
                .collect();
            ev.truncate_warnings(0);
            built = Some(es);
            Value::Null
        }),
    );
    let Some(es) = built else {
        return Value::Object(m);
    };
    let res: Vec<Value> = es
        .iter()
        .map(|e| {
            ex_try(|| match extract_vector_helper(e) {
                None => Value::Null,
                Some(h) => json!({
                    "name": gs(h.distance_fn_name.o.as_bytes()),
                    "pb": h.fn_pb_code,
                    "vec": gs(h.vec.string().as_bytes()),
                    "col": dump(&Expr::Column(h.column)),
                }),
            })
        })
        .collect();
    m.insert("res".into(), Value::Array(res));
    Value::Object(m)
}
