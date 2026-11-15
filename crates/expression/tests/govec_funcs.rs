//! Replays the govec `funcs` vectors (tidb-rs-tracker `govec/domains/funcs.go`): block D08.2
//! part 1 — the `funcs` registry, `newBaseBuiltinFunc*`, `newReturnFieldTypeForBaseBuiltinFunc`
//! and the collation derivation of built-in functions — against Go TiDB v8.5.8.
//!
//! The arguments Go built (columns, constants and small function trees) come as dumps in the
//! output and are rebuilt with descriptor-only signatures, as in the `expr` replay.
//!
//! `GOVEC_FUNCS_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_chunk::Row;
use tidb_expression::builtin::{
    BaseFunctionClass, new_base_builtin_func, new_base_builtin_func_with_field_type,
    new_return_field_type_for_base_builtin_func,
};
use tidb_expression::builtin_registry::{
    get_builtin_list, get_display_name, is_function_supported, registered_builtin_function_names,
    verify_args_wrapper,
};
use tidb_expression::collation::{ExprCollation, derive_collation};
use tidb_expression::function_traits::is_deferred_functions;
use tidb_expression::*;
use tidb_parser::model::CIStr;
use tidb_parser::types::{EvalType, FieldType};
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::collate;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::field_type_builder::new_field_type_builder;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/funcs.jsonl.gz",
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

/// d2DatumInOf's kinds (expr_sf.go).
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
        _ => panic!("datum_in: kind {k}"),
    }
    d
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

fn ex_try(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

fn ex_err(e: &Error) -> Value {
    json!({ "err": gs(&e.error()) })
}

/// Descriptor-only signatures for the trees Go built (as in the `expr` replay).
fn sig_of(name: &str, vectorized: bool) -> &'static eval::SigDesc {
    static SIGS: OnceLock<Mutex<HashMap<(String, bool), &'static eval::SigDesc>>> = OnceLock::new();
    let mut m = SIGS.get_or_init(Default::default).lock().unwrap();
    m.entry((name.to_owned(), vectorized)).or_insert_with(|| {
        Box::leak(Box::new(eval::SigDesc {
            name: Box::leak(name.to_owned().into_boxed_str()),
            pb_code: 0,
            vectorized,
            drivers: eval::SigDrivers::None,
            args_fallback: eval::ArgsFallback::None,
        }))
    })
}

/// A node from sfDump.
fn tree(v: &Value) -> Expr {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        let a = a.as_array().unwrap();
        return Expr::Column(Column {
            unique_id: a[0].as_i64().unwrap(),
            index: a[1].as_i64().unwrap() as isize,
            ret_type: Arc::new(ft_in(&a[2])),
            orig_name: String::from_utf8(ungs(&a[3])).unwrap(),
            ..Column::default()
        });
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1])));
    }
    let a = o["f"].as_array().unwrap();
    let args: Vec<ExprRef> = a[4]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| tree(x).into_ref())
        .collect();
    let ret = ft_in(&a[3]);
    let sig = sig_of(a[2].as_str().unwrap(), a[5].as_bool().unwrap());
    let mut f = eval::BuiltinFunc::new(
        sig,
        args,
        ret.clone(),
        a[6].as_str().unwrap(),
        a[7].as_str().unwrap(),
    );
    f.pb_code = a[8].as_i64().unwrap() as i32;
    if a[9].as_bool().unwrap() {
        f.collation_info
            .set_coercibility(Coercibility(a[10].as_i64().unwrap() as _));
    }
    f.collation_info
        .set_repertoire(Repertoire(a[11].as_i64().unwrap() as _));
    f.collation_info
        .set_explicit_charset(a[12].as_bool().unwrap());
    let mut sf = ScalarFunction::new(a[1].as_str().unwrap(), ret, f);
    sf.func_name = CIStr::new(a[1].as_str().unwrap());
    Expr::ScalarFunction(sf)
}

/// The built arguments (Go's dumps) with the record's coercibility/repertoire overrides.
fn args_of(dumps: &Value, ov: &Value) -> Vec<ExprRef> {
    let mut nodes: Vec<Expr> = dumps.as_array().unwrap().iter().map(tree).collect();
    for o in ov.as_array().unwrap() {
        let o: Vec<i64> = o
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_i64().unwrap())
            .collect();
        let n = &mut nodes[o[0] as usize];
        if o[1] >= 0 {
            match n {
                Expr::Column(c) => c.collation_info.set_coercibility(Coercibility(o[1] as _)),
                Expr::Constant(c) => c.collation_info.set_coercibility(Coercibility(o[1] as _)),
                Expr::ScalarFunction(f) => f.set_coercibility(Coercibility(o[1] as _)),
                _ => unreachable!(),
            }
        }
        if o[2] >= 0 {
            let r = Repertoire(o[2] as _);
            match n {
                Expr::Column(c) => c.collation_info.set_repertoire(r),
                Expr::Constant(c) => c.collation_info.set_repertoire(r),
                Expr::ScalarFunction(f) => f.set_repertoire(r),
                _ => unreachable!(),
            }
        }
    }
    nodes.into_iter().map(Expr::into_ref).collect()
}

fn ec_out(ec: &ExprCollation) -> Value {
    json!([ec.coer.0, ec.repe.0, ec.charset, ec.collation])
}

fn build_ctx(cs: &Value, coll: &Value) -> BuildContextRefT {
    es::new_expr_context(vec![es::with_charset(
        cs.as_str().unwrap().to_owned(),
        coll.as_str().unwrap().to_owned(),
    )])
}

type BuildContextRefT = Arc<es::ExprContext>;

fn exec(op: &str, p: &[Value], want: &Value) -> Value {
    match op {
        "derive" | "newbase" => {
            let prev = collate::new_collation_enabled();
            collate::set_new_collation_enabled_for_test(p[0].as_bool().unwrap());
            let ctx = build_ctx(&p[1], &p[2]);
            let name = p[3].as_str().unwrap();
            let dumps = want["args"].clone();
            let out = if op == "derive" {
                let args = args_of(&dumps, &p[7]);
                let ets: Vec<EvalType> = p[6]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| EvalType(x.as_i64().unwrap() as _))
                    .collect();
                let ret = EvalType(p[5].as_i64().unwrap() as _);
                let ec = ex_try(|| match derive_collation(&*ctx, name, &args, ret, &ets) {
                    Ok(ec) => ec_out(&ec),
                    Err(e) => ex_err(&e),
                });
                json!({ "args": dumps, "ec": ec })
            } else {
                let args = args_of(&dumps, &p[6]);
                let ft = ft_in(&p[5]);
                let with_ft = p[7].as_bool().unwrap();
                let bf = ex_try(|| {
                    let r = if with_ft {
                        new_base_builtin_func_with_field_type(ft, args)
                    } else {
                        new_base_builtin_func(&*ctx, name, args, ft)
                    };
                    match r {
                        Err(e) => ex_err(&e),
                        Ok(bf) => {
                            let ci = &bf.collation_info;
                            let (cs, coll) = ci.charset_and_collation();
                            json!({
                                "tp": ft_out(&bf.tp),
                                "cs": cs,
                                "coll": coll,
                                "coerInit": ci.has_coercibility(),
                                "coer": ci.coercibility().0,
                                "repe": ci.repertoire().0,
                                "explicit": ci.is_explicit_charset(),
                                "key": hex(&bf.collator().key("aB é ß".as_bytes())),
                            })
                        }
                    }
                });
                json!({ "args": dumps, "bf": bf })
            };
            collate::set_new_collation_enabled_for_test(prev);
            out
        }
        "retft" => {
            let e = p[2].as_array().unwrap();
            let ec = ExprCollation {
                coer: Coercibility(e[0].as_i64().unwrap() as _),
                repe: Repertoire(e[1].as_i64().unwrap() as _),
                charset: e[2].as_str().unwrap().to_owned(),
                collation: e[3].as_str().unwrap().to_owned(),
            };
            let name = p[0].as_str().unwrap();
            let et = EvalType(p[1].as_i64().unwrap() as _);
            ex_try(|| ft_out(&new_return_field_type_for_base_builtin_func(name, et, &ec)))
        }
        "verify" => {
            let err_out = |r: Result<(), Error>| match r {
                Ok(()) => Value::Null,
                Err(e) => ex_err(&e),
            };
            let name: &'static str = Box::leak(p[0].as_str().unwrap().to_owned().into_boxed_str());
            let cls = BaseFunctionClass {
                func_name: name,
                min_args: p[1].as_i64().unwrap() as i32,
                max_args: p[2].as_i64().unwrap() as i32,
            };
            let n = p[3].as_u64().unwrap() as usize;
            let nils: Vec<ExprRef> = (0..n)
                .map(|_| Expr::Column(Column::default()).into_ref())
                .collect();
            json!({
                "wrapper": err_out(verify_args_wrapper(name, n)),
                "args": err_out(cls.verify_args(&nils)),
            })
        }
        "setflen" => {
            let mut bf = eval::BuiltinFunc::new(
                &tidb_expression::builtin::baseBuiltinFuncSig,
                vec![],
                ft_in(&p[0]),
                "",
                "binary",
            );
            let fsp = p[2].as_i64().unwrap() as i32;
            match p[1].as_i64().unwrap() {
                0 => bf.set_decimal_and_flen_for_datetime(fsp),
                1 => bf.set_decimal_and_flen_for_date(),
                _ => bf.set_decimal_and_flen_for_time(fsp),
            }
            ft_out(&bf.tp)
        }
        "registry" => {
            let names = registered_builtin_function_names();
            let mut probes: Vec<&str> = names.clone();
            probes.extend(["no_such_function", "GE", "", "'tidb`.(dateliteral"]);
            json!({
                "names": names,
                "list": get_builtin_list(),
                "probes": probes,
                "supported": probes.iter().map(|n| is_function_supported(n)).collect::<Vec<_>>(),
                "display": probes.iter().map(|n| get_display_name(n)).collect::<Vec<_>>(),
            })
        }
        "baseerr" => {
            let ctx = es::new_eval_context(vec![]);
            let mut bf = eval::BuiltinFunc::new(
                &tidb_expression::builtin::baseBuiltinFuncSig,
                vec![],
                FieldType::default(),
                "",
                "binary",
            );
            let st = EvalState::new();
            let e = |r: Result<(), Error>| gs(&r.unwrap_err().error());
            let ev = vec![
                e(bf.eval_int(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_real(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_string(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_decimal(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_time(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_duration(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_json(&*ctx, &st, Row::default()).map(|_| ())),
                e(bf.eval_vector_float32(&*ctx, &st, Row::default())
                    .map(|_| ())),
            ];
            let mut vecs = vec![];
            for et in [
                tidb_parser::types::ETInt,
                tidb_parser::types::ETReal,
                tidb_parser::types::ETString,
                tidb_parser::types::ETDecimal,
                tidb_parser::types::ETDatetime,
                tidb_parser::types::ETDuration,
                tidb_parser::types::ETJson,
                tidb_parser::types::ETVectorFloat32,
            ] {
                let mut res = tidb_chunk::column::new_empty_column(&FieldType::default());
                vecs.push(e(bf.vec_eval(&*ctx, et, 0, &[], &mut res)));
            }
            bf.set_pb_code(tipb_plus_int());
            json!({
                "eval": ev,
                "vec": vecs,
                "metadata": bf.metadata().is_none(),
                "props": bf.required_optional_eval_props().0,
                "pb": bf.pb_code(),
            })
        }
        "deferred" => {
            let ctx = es::new_expr_context(vec![es::with_sys_date_is_now(p[1].as_bool().unwrap())]);
            json!(is_deferred_functions(&*ctx, p[0].as_str().unwrap()))
        }
        _ => panic!("unknown op {op}"),
    }
}

/// tipb.ScalarFuncSig_PlusInt.
fn tipb_plus_int() -> i32 {
    203
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "funcs");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap();
        let got = exec(op, &args, &rec["out"]);
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
fn govec_funcs() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_FUNCS_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
