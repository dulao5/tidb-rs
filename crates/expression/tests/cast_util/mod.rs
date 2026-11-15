//! The D08.12 batch 1 op of the govec `cast` domain (tidb-rs-tracker
//! `govec/domains/cast_d0812.go`): "util" runs the expression tree utilities of util.go over
//! expression recipes and dumps their results.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use tidb_base::contextutil::WarnHandler;
use tidb_expression::ast_funcs as ast;
use tidb_expression::schema::new_schema;
use tidb_expression::util::*;
use tidb_expression::*;

use super::cast_eval::expr_of;
use super::{Ctxs, ctx_of, dump, ex_try, ft_in, gs, warns};

fn dump_opt(e: &Option<ExprRef>) -> Value {
    e.as_ref().map_or(Value::Null, |e| dump(e))
}

fn dump_list(es: &[ExprRef]) -> Value {
    Value::Array(es.iter().map(|e| dump(e)).collect())
}

fn cols(cs: &[Arc<Column>]) -> Value {
    Value::Array(
        cs.iter()
            .map(|c| dump(&Expr::Column((**c).clone())))
            .collect(),
    )
}

/// utIDs: the UniqueIDs only (which column of one UniqueID survives the sort is
/// unspecified).
fn ids(cs: &[Arc<Column>]) -> Value {
    json!(cs.iter().map(|c| c.unique_id).collect::<Vec<_>>())
}

/// utSorted: the dumps as JSON text, sorted (Go sorts the marshalled strings).
fn sorted(es: &[ExprRef]) -> Value {
    let mut ss: Vec<String> = es.iter().map(|e| go_json(&dump(e))).collect();
    ss.sort();
    Value::Array(
        ss.iter()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect(),
    )
}

/// Go's json.Marshal of a dump: object keys sorted, no spaces, HTML characters escaped.
fn go_json(v: &Value) -> String {
    // serde_json's Map is ordered by key (no "preserve_order"), as Go's map marshalling.
    serde_json::to_string(v)
        .unwrap()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn schema_cols(fts: &[tidb_parser::types::FieldType], n: usize) -> Vec<Arc<Column>> {
    (0..n.min(fts.len()))
        .map(|i| {
            Arc::new(Column {
                unique_id: i as i64 + 1,
                index: i as isize,
                ret_type: Arc::new(fts[i].clone()),
                orig_name: "c".into(),
                ..Column::default()
            })
        })
        .collect()
}

pub(super) fn exec(op: &str, p: &[Value]) -> Option<Value> {
    if op != "util" {
        return None;
    }
    let kind = p[0].as_str().unwrap();
    let ctx = ctx_of(&p[2]);
    let ev = &ctx.1;
    let fts: Vec<_> = p[3].as_array().unwrap().iter().map(ft_in).collect();
    let mut m = Map::new();
    let mut built: Option<Vec<ExprRef>> = None;
    m.insert(
        "args".into(),
        ex_try(|| {
            let es: Vec<ExprRef> = p[1]
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
        return Some(Value::Object(m));
    };
    let bctx = &*ctx.0;
    match kind {
        "x" => exec_x(&mut m, &ctx, &es, p, &fts),
        "pnot" => {
            let res: Vec<Value> = es
                .iter()
                .map(|e| {
                    json!({
                        "pnot": ex_try(|| dump_opt(&push_down_not(bctx, e))),
                        "elim": ex_try(|| dump_opt(&eliminate_no_precision_loss_cast(bctx, e))),
                        "truth": dump(&get_expr_inside_is_truth(e)),
                        "outnot": contain_outer_not(e),
                    })
                })
                .collect();
            m.insert("res".into(), Value::Array(res));
        }
        "dnf" => {
            let n_schema = p[4].as_u64().unwrap() as usize;
            let schema = new_schema(schema_cols(&fts, n_schema));
            let mut res = Vec::new();
            for e in &es {
                let mut r = Map::new();
                r.insert(
                    "relax".into(),
                    ex_try(|| dump_opt(&derive_relaxed_filters_from_dnf(bctx, e, &schema))),
                );
                if let Expr::ScalarFunction(sf) = &**e
                    && sf.func_name.l == ast::LogicOr
                {
                    r.insert(
                        "dnf".into(),
                        ex_try(|| {
                            let (ex, rem) = extract_filters_from_dnf(bctx, sf, e);
                            json!([sorted(&ex), dump_opt(&rem)])
                        }),
                    );
                }
                res.push(Value::Object(r));
            }
            m.insert("res".into(), Value::Array(res));
            m.insert(
                "all".into(),
                ex_try(|| sorted(&extract_filters_from_dnfs(bctx, es.clone()))),
            );
        }
        "const" => {
            let res: Vec<Value> = es
                .iter()
                .map(|e| {
                    let mut r = Map::new();
                    r.insert(
                        "str".into(),
                        ex_try(|| match get_string_from_constant(&**ev, e) {
                            Err(err) => json!({ "err": gs(&err.error()) }),
                            Ok((s, is_null)) => json!([gs(&s), is_null]),
                        }),
                    );
                    r.insert(
                        "int".into(),
                        ex_try(|| match get_int_from_constant(&**ev, e) {
                            Err(err) => json!({ "err": gs(&err.error()) }),
                            Ok((v, is_null)) => json!([v, is_null]),
                        }),
                    );
                    r.insert(
                        "u64".into(),
                        ex_try(|| {
                            let (v, is_null, ok) = get_uint64_from_constant(&**ev, e);
                            json!([v, is_null, ok])
                        }),
                    );
                    if let Expr::Constant(c) = &**e {
                        let d = datum_to_constant(
                            c.value.clone(),
                            c.ret_type.get_type(),
                            c.ret_type.get_flag(),
                        );
                        r.insert("d2c".into(), dump(&Expr::Constant(d)));
                    }
                    Value::Object(r)
                })
                .collect();
            m.insert("res".into(), Value::Array(res));
        }
        "fmt" => {
            let res: Vec<Value> = p[4]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| {
                    let f = f64::from_bits(b.as_u64().unwrap());
                    json!([get_format_bytes(f), get_format_nano_time(f)])
                })
                .collect();
            m.insert("res".into(), Value::Array(res));
        }
        k => panic!("util kind {k}"),
    }
    m.insert("warns".into(), warns(&ev.truncate_warnings(0)));
    Some(Value::Object(m))
}

fn exec_x(
    m: &mut Map<String, Value>,
    ctx: &Ctxs,
    es: &[ExprRef],
    p: &[Value],
    fts: &[tidb_parser::types::FieldType],
) {
    let bctx = &*ctx.0;
    let ev = &*ctx.1;
    let even = |c: &Column| c.unique_id % 2 == 0;
    let per: Vec<Value> = es
        .iter()
        .map(|e| {
            let (col, ub) = find_upper_bound(e);
            let col_out = col.map_or(Value::Null, |c| dump(&Expr::Column((*c).clone())));
            let cors: Vec<Value> = extract_cor_columns(e)
                .iter()
                .map(|c| dump(&Expr::CorrelatedColumn((**c).clone())))
                .collect();
            json!({
                "cols": cols(&extract_columns(e)),
                "dep": cols(&extract_dependent_columns(e)),
                "cor": cors,
                "ub": [col_out, ub],
                "rtc": is_runtime_const_expr(e),
                "nondet": check_non_deterministic(e),
                "inplus": check_func_in_expr(e, ast::Plus),
                "incast": check_func_in_expr(e, ast::Cast),
                "immut": is_immutable_func(e),
                "mut": is_mutable_effects_expr(e),
                "sv": expr_has_set_var_or_sleep(e),
                "rowlen": get_row_len(e),
                "arg0": ex_try(|| dump_opt(&get_func_arg(e, 0))),
                "cpc": [const_expr_consider_plan_cache(e, false), const_expr_consider_plan_cache(e, true)],
            })
        })
        .collect();
    m.insert("per".into(), Value::Array(per));
    let mut all = Map::new();
    all.insert(
        "filter".into(),
        dump_list(&filter(Vec::new(), es, |e| {
            matches!(&**e, Expr::ScalarFunction(_))
        })),
    );
    let (rem, out) = filter_out_in_place(es.to_vec(), |e| matches!(&**e, Expr::Column(_)));
    all.insert("fout".into(), json!([dump_list(&rem), dump_list(&out)]));
    all.insert(
        "used".into(),
        ids(&extract_all_columns_from_expressions_in_used_slices(
            Vec::new(),
            None,
            es,
        )),
    );
    all.insert(
        "usedeven".into(),
        ids(&extract_all_columns_from_expressions_in_used_slices(
            Vec::new(),
            Some(&even),
            es,
        )),
    );
    all.insert(
        "fromexprs".into(),
        cols(&extract_columns_from_expressions(Vec::new(), es, None)),
    );
    all.insert(
        "fromeven".into(),
        cols(&extract_columns_from_expressions(
            Vec::new(),
            es,
            Some(&even),
        )),
    );
    let eq: Vec<Value> = extract_equivalence_columns(Vec::new(), es)
        .iter()
        .map(|pr| dump_list(pr))
        .collect();
    all.insert("eqv".into(), Value::Array(eq));
    all.insert(
        "colcor".into(),
        cols(&extract_columns_and_cor_columns_from_expressions(
            Vec::new(),
            es,
        )),
    );
    all.insert(
        "consteq".into(),
        ex_try(|| dump_list(&extract_constant_eq_columns_or_scalar(bctx, Vec::new(), es))),
    );
    all.insert(
        "set".into(),
        json!(extract_column_set(es).into_iter().collect::<Vec<_>>()),
    );
    if !es.is_empty() {
        all.insert(
            "contains".into(),
            json!([
                contains(ev, &es[..es.len() - 1], &es[es.len() - 1]),
                contains(ev, es, &es[0])
            ]),
        );
    }
    all.insert("vcol".into(), json!(contain_virtual_column(es)));
    all.insert("corcol".into(), json!(contain_correlated_column(es)));
    all.insert(
        "proj".into(),
        json!([
            projection_benefits_from_pushed_down(es, es.len()),
            projection_benefits_from_pushed_down(es, es.len() + 1)
        ]),
    );
    all.insert("side".into(), json!(exprs_has_side_effects(es)));
    all.insert(
        "multirow".into(),
        ex_try(|| match check_args_not_multi_column_row(es) {
            Err(e) => json!({ "err": gs(&e.error()) }),
            Ok(()) => Value::Null,
        }),
    );
    all.insert("disp".into(), json!(exprs_to_strings_for_display(ev, es)));
    all.insert("dedup".into(), dump_list(&remove_dup_exprs(es.to_vec())));
    all.insert(
        "mo".into(),
        json!(maybe_over_optimized4_plan_cache_for_multi_expression(
            bctx, es
        )),
    );
    let n_schema = p[4].as_u64().unwrap() as usize;
    if n_schema > 0 && !es.is_empty() {
        let cs = schema_cols(fts, n_schema);
        let news: Vec<ExprRef> = (0..n_schema)
            .map(|i| Arc::clone(&es[i % es.len()]))
            .collect();
        all.insert(
            "csall".into(),
            ex_try(|| {
                let (fail, r) = column_substitute_all(bctx, &es[0], &new_schema(cs), &news);
                json!([fail, dump(&r)])
            }),
        );
    }
    m.insert("all".into(), Value::Object(all));
}
