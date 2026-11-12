//! The constant propagation op of the govec `cast` domain (tidb-rs-tracker
//! `govec/domains/cast_d082c.go`): "cprop" runs `PropagateConstant` (variant 0) or
//! `PropConstOverOuterJoin` (variant 1) over condition recipes and dumps the resulting
//! condition lists.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tidb_base::contextutil::WarnHandler;
use tidb_expression::constant_propagation::{prop_const_over_outer_join, propagate_constant};
use tidb_expression::schema::new_schema;
use tidb_expression::*;
use tidb_sessionctx::exprctx::ExprContextRef;

use super::cast_eval::expr_of;
use super::{ctx_of, dump, ex_try, ft_in, warns};

fn dump_list(es: &[ExprRef]) -> Value {
    Value::Array(es.iter().map(|e| dump(e)).collect())
}

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    if op != "cprop" {
        return None;
    }
    let ctx = ctx_of(&p[3]);
    let ev = &ctx.1;
    let fts: Vec<_> = p[4].as_array().unwrap().iter().map(ft_in).collect();
    let mut m = Map::new();
    let mut built: Option<(Vec<ExprRef>, Vec<ExprRef>)> = None;
    m.insert(
        "args".into(),
        ex_try(|| {
            let of = |v: &Value| -> Vec<ExprRef> {
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|r| expr_of(&ctx, r, &fts))
                    .collect()
            };
            let conds = of(&p[1]);
            let filters = of(&p[2]);
            ev.truncate_warnings(0);
            built = Some((conds, filters));
            Value::Null
        }),
    );
    let Some((conds, filters)) = built else {
        return Some(Value::Object(m));
    };
    let ectx: ExprContextRef = ctx.0.clone();
    m.insert(
        "res".into(),
        ex_try(|| {
            if p[0].as_i64().unwrap() == 0 {
                return dump_list(&propagate_constant(&ectx, &conds));
            }
            let n_outer = p[5].as_u64().unwrap() as usize;
            let (mut outer, mut inner) = (Vec::new(), Vec::new());
            for (i, ft) in fts.iter().enumerate() {
                let c = Arc::new(Column {
                    unique_id: i as i64 + 1,
                    index: i as isize,
                    ret_type: ft.clone(),
                    orig_name: "c".into(),
                    ..Column::default()
                });
                if i < n_outer {
                    outer.push(c);
                } else {
                    inner.push(c);
                }
            }
            let (j, f) = prop_const_over_outer_join(
                &ectx,
                &conds,
                &filters,
                &new_schema(outer),
                &new_schema(inner),
                p[6].as_bool().unwrap(),
            );
            json!({ "join": dump_list(&j), "filter": dump_list(&f) })
        }),
    );
    m.insert("warns".into(), warns(&ev.truncate_warnings(0)));
    Some(Value::Object(m))
}
