//! Replays the govec `rangerbuild` vectors (D10.4, VS1-25: `pkg/util/ranger` checker.go,
//! detacher.go, points.go, ranger.go) against Go TiDB v8.5.8 (tracker
//! `govec/domains/rangerbuild.go`, which documents the record format).
//!
//! Go plans each record's query and records the conditions and index columns it built
//! ("in"); this replay rebuilds them with descriptor-only signatures (no evaluation: the
//! ranger only reads function names, types and collations, and evaluates constants), runs the
//! ported ranger and compares every result with Go's.
//!
//! Boundaries (spec D10.4 §5): rebuilding an `in(...)` condition (`points2EqOrInCond`) and
//! evaluating `tidb_shard()` (`AddGcColumn4*`) need D08.9; such a result is
//! `blocked-by:D08.9` in tidb-rs and is counted (pinned in `EXPECTED_BLOCKED`).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::{blocked, blocked_by};
use tidb_base::errors::RedactLogDisable;
use tidb_base::gostd::time::utc;
use tidb_expression::eval::{self, BuiltinFunc};
use tidb_expression::expression::flatten_dnf_conditions;
use tidb_expression::{Column, Constant, CorrelatedColumn, Expr, ExprRef, ScalarFunction};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_planner::ranger::{self, DetachRangeResult, ValueInfo};
use tidb_sessionctx::exprctx::{BuildContextRef, EvalContext};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::ranger::types::{Range, Ranges};
use tidb_sessionctx::stmtctx::{StatementContext, new_stmt_ctx_with_time_zone};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate::{self, Collator};
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::parse_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The results that stop at a boundary, per `op.result:block`, in the committed vectors (the
/// same on both targets; constitution §7.3). All are D08.9: 25 `index` records whose
/// conditions merge into an `in(...)` that `points2EqOrInCond` rebuilds (each of the four
/// results of the record stops there), and 21 shard indexes whose `AddGcColumn4*` evaluates
/// `tidb_shard()`. A boundary that starts answering, or a new one, fails here: update the pin
/// in the same PR.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    ("index.eqin:D08.9", 25),
    ("index.idx:D08.9", 25),
    ("index.part:D08.9", 25),
    ("index.simple:D08.9", 25),
    ("shard.add:D08.9", 21),
];

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/rangerbuild.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- values

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

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

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
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

fn bits_f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

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
        dm::KindMysqlDecimal => {
            // The generator adds the decimal's state (MarshalJSON): constant folding leaves
            // states that FromString does not give (digitsInt rounded to a word).
            let mut dec = dec_of(p[1].as_str().unwrap());
            if let Some(st) = p.get(2) {
                dec.unmarshal_json(st.as_str().unwrap().as_bytes()).unwrap();
            }
            d.set_mysql_decimal(dec)
        }
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

fn datum_out(d: &Datum) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("k".into(), json!(d.kind()));
    m.insert("frac".into(), json!(d.frac()));
    m.insert("len".into(), json!(d.length()));
    m.insert("i".into(), json!(d.get_int64()));
    m.insert("coll".into(), gs(d.collation().as_bytes()));
    m.insert("b".into(), json!(hex(d.get_bytes())));
    if d.kind() == dm::KindRaw && d.raw_b().is_none() {
        m.insert("raw_nil".into(), json!(true));
    }
    match d.kind() {
        dm::KindMysqlDecimal => {
            m.insert("dec".into(), dec_out(d.get_mysql_decimal()));
        }
        dm::KindMysqlTime => {
            let t = d.get_mysql_time();
            m.insert(
                "time".into(),
                json!({ "raw": t.raw().0, "s": gs(&t.string()) }),
            );
        }
        _ => {}
    }
    Value::Object(m)
}

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

// ---------------------------------------------------------------- expressions

/// The error of evaluating a descriptor-only signature: the families are not ported here
/// (`tidb_shard` is D08.9).
fn sig_unsupported(f: &BuiltinFunc, _: &dyn EvalContext) -> Error {
    let block = match f.sig.name {
        "builtinTidbShardSig" => "D08.9",
        _ => "D08.x",
    };
    blocked(block, f.sig.name)
}

/// Descriptor-only signatures for the trees Go built (as the `expr` replay): Go's signature
/// type name; evaluating one is a boundary error.
fn sig_of(name: &str) -> &'static eval::SigDesc {
    static SIGS: OnceLock<Mutex<HashMap<String, &'static eval::SigDesc>>> = OnceLock::new();
    let mut m = SIGS.get_or_init(Default::default).lock().unwrap();
    m.entry(name.to_owned()).or_insert_with(|| {
        Box::leak(Box::new(eval::SigDesc {
            name: Box::leak(name.to_owned().into_boxed_str()),
            pb_code: 0,
            vectorized: false,
            drivers: eval::SigDrivers::Unsupported {
                err: sig_unsupported,
            },
            args_fallback: eval::ArgsFallback::None,
        }))
    })
}

fn col_of(a: &[Value]) -> Column {
    Column {
        id: a[0].as_i64().unwrap(),
        unique_id: a[1].as_i64().unwrap(),
        index: a[2].as_i64().unwrap() as isize,
        ret_type: Arc::new(ft_in(&a[3])),
        orig_name: String::from_utf8(ungs(&a[4])).unwrap(),
        is_hidden: a[5].as_bool().unwrap(),
        virtual_expr: a.get(6).filter(|x| !x.is_null()).map(tree),
        ..Column::default()
    }
}

fn tree(v: &Value) -> ExprRef {
    let o = v.as_object().unwrap();
    if let Some(a) = o.get("col") {
        return Expr::Column(col_of(a.as_array().unwrap())).into_ref();
    }
    if let Some(a) = o.get("corr") {
        let a = a.as_array().unwrap();
        return Expr::CorrelatedColumn(CorrelatedColumn::new(Column {
            id: a[0].as_i64().unwrap(),
            unique_id: a[1].as_i64().unwrap(),
            ret_type: Arc::new(ft_in(&a[2])),
            orig_name: String::from_utf8(ungs(&a[3])).unwrap(),
            ..Column::default()
        }))
        .into_ref();
    }
    if let Some(a) = o.get("const") {
        let a = a.as_array().unwrap();
        return Expr::Constant(Constant::new(datum_in(&a[0]), ft_in(&a[1]))).into_ref();
    }
    let a = o["f"].as_array().unwrap();
    let args: Vec<ExprRef> = a[4].as_array().unwrap().iter().map(tree).collect();
    let ret = ft_in(&a[3]);
    let f = BuiltinFunc::new(
        sig_of(a[2].as_str().unwrap()),
        args,
        ret.clone(),
        a[5].as_str().unwrap(),
        a[6].as_str().unwrap(),
    );
    let mut sf = ScalarFunction::new(a[1].as_str().unwrap(), ret, f);
    sf.func_name = CIStr::new(a[1].as_str().unwrap());
    Expr::ScalarFunction(sf).into_ref()
}

fn trees(v: &Value) -> Vec<ExprRef> {
    v.as_array().unwrap().iter().map(tree).collect()
}

fn cols(v: &Value) -> Vec<Arc<Column>> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|c| Arc::new(col_of(c["col"].as_array().unwrap())))
        .collect()
}

// ---------------------------------------------------------------- context and outputs

struct Env {
    sc: Arc<StatementContext>,
    rctx: RangerContext,
}

fn env(opts: Option<&Value>) -> Env {
    collate::set_new_collation_enabled_for_test(true);
    let sc = Arc::new(new_stmt_ctx_with_time_zone(utc()));
    let expr_ctx: BuildContextRef = es::new_expr_context(vec![]);
    let b = |k: &str, d: bool| opts.and_then(|o| o[k].as_bool()).unwrap_or(d);
    let mut fix = HashMap::new();
    if b("f44389", false) {
        fix.insert(44389u64, "ON".to_string());
    }
    if b("f54337", false) {
        fix.insert(54337u64, "ON".to_string());
    }
    let rctx = RangerContext {
        type_ctx: sc.type_ctx(),
        err_ctx: sc.err_ctx(),
        expr_ctx,
        range_fallback_handler: Arc::clone(&sc.range_fallback_handler),
        plan_cache_tracker: Arc::clone(&sc.plan_cache_tracker),
        optimizer_fix_control: fix,
        use_cache: false,
        in_prepared_plan_building: false,
        regard_null_as_point: b("np", true),
        opt_prefix_index_single_scan: b("ps", true),
    };
    Env { sc, rctx }
}

fn max_of(opts: &Value) -> i64 {
    opts["max"].as_i64().unwrap_or(0)
}

fn warns(e: &Env) -> Value {
    Value::Array(
        e.sc.get_warnings()
            .iter()
            .map(|w| gs(&w.err.error()))
            .collect(),
    )
}

/// The collator's name: the first name whose collator is the same value (as Go's interface
/// comparison), "#derived" for `GetBinaryCollator()`.
fn coll_name(c: &'static dyn Collator) -> Value {
    use tidb_sessionctx::ranger::types::same_collator;
    if same_collator(c, collate::get_binary_collator()) {
        return json!("#derived");
    }
    for n in [
        "binary",
        "utf8mb4_bin",
        "utf8mb4_general_ci",
        "utf8mb4_unicode_ci",
        "utf8mb4_0900_ai_ci",
        "utf8mb4_0900_bin",
        "utf8_bin",
        "utf8_general_ci",
        "utf8_unicode_ci",
        "ascii_bin",
        "latin1_bin",
        "gbk_bin",
        "gbk_chinese_ci",
        "gb18030_bin",
        "gb18030_chinese_ci",
        "utf8mb4_zh_pinyin_tidb_as_cs",
    ] {
        if same_collator(c, collate::get_collator(n)) {
            return json!(n);
        }
    }
    json!("#unknown")
}

fn range_out(r: &Range) -> Value {
    json!({
        "s": gs(&r.string()),
        "l": r.low_val.iter().map(datum_out).collect::<Vec<_>>(),
        "h": r.high_val.iter().map(datum_out).collect::<Vec<_>>(),
        "le": r.low_exclude,
        "he": r.high_exclude,
        "c": r.collators.iter().map(|c| coll_name(*c)).collect::<Vec<_>>(),
    })
}

fn ranges_out(rs: &[Range]) -> Value {
    Value::Array(rs.iter().map(range_out).collect())
}

fn strs(es: &[ExprRef]) -> Value {
    let ectx = es::new_eval_context(vec![]);
    Value::Array(
        es.iter()
            .map(|e| gs(&e.string_with_ctx(&*ectx, RedactLogDisable)))
            .collect(),
    )
}

fn cv_out(cv: &Option<Vec<Option<ValueInfo>>>) -> Value {
    match cv {
        None => Value::Null,
        Some(cv) => Value::Array(
            cv.iter()
                .map(|v| match v {
                    None => Value::Null,
                    Some(v) if v.mutable => json!({ "m": true }),
                    Some(v) => json!({ "v": datum_out(v.value.as_ref().unwrap()) }),
                })
                .collect(),
        ),
    }
}

/// A result: a JSON value to compare, or a boundary.
enum Got {
    Value(Value),
    Blocked(String),
}

fn err_out(e: &Env, err: Error) -> Got {
    if let Some(b) = blocked_by(&err) {
        return Got::Blocked(b.to_string());
    }
    Got::Value(json!({ "err": gs(&err.error()), "w": warns(e) }))
}

/// M-ERR: `errors.Wrap` is not ported, so `Datum.ConvertTo`'s "convert to MySQL set / enum
/// failed: ...: " prefix is not in tidb-rs's message (D01.1; the cause is the same).
fn wrap_equal(g: &Value, w: &Value) -> bool {
    let (Some(ge), Some(we)) = (g.get("err"), w.get("err")) else {
        return false;
    };
    let (Some(ge), Some(we)) = (ge.as_str(), we.as_str()) else {
        return false;
    };
    (we.starts_with("convert to MySQL set failed: ")
        || we.starts_with("convert to MySQL enum failed: "))
        && we.ends_with(&format!(": {ge}"))
        && g.get("w") == w.get("w")
}

fn detach_out(e: &Env, r: Result<DetachRangeResult, Error>) -> Got {
    match r {
        Err(err) => err_out(e, err),
        Ok(res) => Got::Value(json!({
            "r": ranges_out(&res.ranges),
            "a": strs(&res.access_conds),
            "f": strs(&res.remained_conds),
            "eq": res.eq_cond_count,
            "eqin": res.eq_or_in_count,
            "dnf": res.is_dnf_cond,
            "mdnf": res.min_access_conds_for_dnf_cond,
            "cv": cv_out(&res.column_values),
            "w": warns(e),
        })),
    }
}

fn caught(f: impl FnOnce() -> Got) -> Got {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(g) => g,
        Err(_) => Got::Value(json!({ "panic": true })),
    }
}

// ---------------------------------------------------------------- ops

/// Each sub-result of a record: (key, result).
fn exec(op: &str, p: &[Value], want: &Value) -> Vec<(String, Got)> {
    let mut out: Vec<(String, Got)> = Vec::new();
    match op {
        "index" => {
            if want.get("plan").is_some() {
                return out;
            }
            let opts = &p[5];
            let conds = trees(&want["in"]["conds"]);
            let cs = cols(&want["in"]["cols"]);
            if cs.is_empty() {
                return out;
            }
            let lengths: Vec<isize> = want["in"]["lengths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l.as_i64().unwrap() as isize)
                .collect();
            let max = max_of(opts);
            out.push((
                "idx".into(),
                caught(|| {
                    let e = env(Some(opts));
                    let r = ranger::detach_cond_and_build_range_for_index(
                        &e.rctx, &conds, &cs, &lengths, max,
                    );
                    detach_out(&e, r)
                }),
            ));
            out.push((
                "simple".into(),
                caught(|| {
                    let e = env(Some(opts));
                    match ranger::detach_simple_cond_and_build_range_for_index(
                        &e.rctx, &conds, &cs, &lengths, max,
                    ) {
                        Err(err) => err_out(&e, err),
                        Ok((rs, a, f)) => Got::Value(json!({
                            "r": ranges_out(&rs), "a": strs(&a), "f": strs(&f), "w": warns(&e),
                        })),
                    }
                }),
            ));
            out.push((
                "part".into(),
                caught(|| {
                    let e = env(Some(opts));
                    let r = ranger::detach_cond_and_build_range_for_partition(
                        &e.rctx, &conds, &cs, &lengths, max,
                    );
                    detach_out(&e, r)
                }),
            ));
            out.push((
                "eqin".into(),
                caught(|| {
                    let e = env(Some(opts));
                    match ranger::extract_eq_and_in_condition(&e.rctx, &conds, &cs, &lengths) {
                        Err(err) => err_out(&e, err),
                        Ok(None) => Got::Value(json!({ "empty": true })),
                        Ok(Some((a, f, n, cv))) => Got::Value(json!({
                            "a": strs(&a), "f": strs(&f), "n": strs(&n), "cv": cv_out(&Some(cv)),
                        })),
                    }
                }),
            ));
        }
        "column" => {
            if want.get("plan").is_some() {
                return out;
            }
            let opts = &p[7];
            let conds = trees(&want["in"]["conds"]);
            let col = Arc::new(col_of(want["in"]["col"]["col"].as_array().unwrap()));
            let length = p[5].as_i64().unwrap() as isize;
            let max = p[6].as_i64().unwrap();
            let e0 = env(Some(opts));
            let ext = ranger::extract_access_conditions_for_column(&e0.rctx, &conds, &col);
            out.push(("ext".into(), Got::Value(strs(&ext))));
            out.push((
                "cr".into(),
                caught(|| {
                    let e = env(Some(opts));
                    match ranger::build_column_range(
                        ext.clone(),
                        &e.rctx,
                        &col.ret_type,
                        length,
                        max,
                    ) {
                        Err(err) => err_out(&e, err),
                        Ok((rs, a, f)) => Got::Value(json!({
                            "r": ranges_out(&rs), "a": strs(&a), "f": strs(&f), "w": warns(&e),
                        })),
                    }
                }),
            ));
            let (access, filter) = ranger::detach_conds_for_column(&e0.rctx, &conds, &col);
            out.push((
                "det".into(),
                Got::Value(json!({ "a": strs(&access), "f": strs(&filter) })),
            ));
            out.push((
                "tr".into(),
                caught(|| {
                    let e = env(Some(opts));
                    match ranger::build_table_range(access.clone(), &e.rctx, &col.ret_type, max) {
                        Err(err) => err_out(&e, err),
                        Ok((rs, a, f)) => Got::Value(json!({
                            "r": ranges_out(&rs), "a": strs(&a), "f": strs(&f), "w": warns(&e),
                        })),
                    }
                }),
            ));
        }
        "merge_dnf" => {
            if want.get("plan").is_some() {
                return out;
            }
            let e = env(None);
            let conds = trees(&want["in"]["conds"]);
            let mut res = Vec::new();
            for c in &conds {
                let Expr::ScalarFunction(sf) = &**c else {
                    continue;
                };
                if sf.func_name.l != "or" {
                    continue;
                }
                let merged = ranger::merge_dnf_items4_col(&e.rctx, &flatten_dnf_conditions(sf));
                let mut s: Vec<Value> = strs(&merged).as_array().unwrap().clone();
                s.sort_by_key(ungs);
                res.push(Value::Array(s));
            }
            out.push(("merged".into(), Got::Value(Value::Array(res))));
        }
        "shard" => {
            if want.get("plan").is_some() {
                return out;
            }
            let conds = trees(&want["in"]["conds"]);
            for (i, x) in want["idx"].as_array().unwrap().iter().enumerate() {
                let cs = cols(&x["cols"]);
                out.push((
                    format!("idx{i}.valid"),
                    Got::Value(json!(ranger::is_valid_shard_index(&cs))),
                ));
                if cs.is_empty() {
                    continue;
                }
                let sf = cs[0]
                    .virtual_expr
                    .as_deref()
                    .and_then(|e| e.as_scalar_function());
                let fields: Vec<ExprRef> = ranger::extract_columns_from_expr(sf)
                    .into_iter()
                    .map(|c| Expr::Column(Column::clone(&c)).into_ref())
                    .collect();
                out.push((format!("idx{i}.fields"), Got::Value(strs(&fields))));
                out.push((
                    format!("idx{i}.add"),
                    caught(|| {
                        let e = env(None);
                        match ranger::add_expr4_eq_and_in_condition(&e.rctx, &conds, &cs) {
                            Err(err) => err_out(&e, err),
                            Ok(nc) => Got::Value(strs(&nc)),
                        }
                    }),
                ));
            }
        }
        "union" => {
            let rs = ranges_in(&p[0]);
            out.push((
                "".into(),
                caught(|| {
                    let e = env(None);
                    match ranger::union_ranges(&e.rctx, rs, p[1].as_bool().unwrap()) {
                        Err(err) => err_out(&e, err),
                        Ok(r) => Got::Value(json!({ "r": ranges_out(&r) })),
                    }
                }),
            ));
        }
        "append" => {
            let (pr, rs) = (ranges_in(&p[0]), ranges_in(&p[1]));
            let (r, fb) = ranger::append_ranges2_point_ranges(pr, &rs, p[2].as_i64().unwrap());
            out.push((
                "".into(),
                Got::Value(json!({ "r": ranges_out(&r), "fb": fb })),
            ));
        }
        "to_string" => {
            let rs = ranges_in(&p[0]);
            let names: Vec<String> = p[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            out.push((
                "".into(),
                caught(|| {
                    let e = env(None);
                    match ranger::ranges_to_string(&e.sc, &rs, &names) {
                        Err(err) => Got::Value(json!({ "err": gs(&err.error()) })),
                        Ok(s) => Got::Value(gs(&s)),
                    }
                }),
            ));
        }
        "cut" => {
            let mut d = datum_in(&p[0]);
            let length = p[1].as_i64().unwrap() as isize;
            let ft = ft_in(&p[2]);
            let reach = ranger::reach_prefix_len(&d, length, &ft);
            let cut = ranger::cut_datum_by_prefix_len(&mut d, length, &ft);
            out.push((
                "".into(),
                Got::Value(json!({ "reach": reach, "cut": cut, "d": datum_out(&d) })),
            ));
        }
        "null_range" => out.push(("".into(), Got::Value(ranges_out(&ranger::null_range())))),
        _ => panic!("unknown op {op}"),
    }
    out
}

fn ranges_in(v: &Value) -> Ranges {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| Range {
            low_val: r["l"].as_array().unwrap().iter().map(datum_in).collect(),
            high_val: r["h"].as_array().unwrap().iter().map(datum_in).collect(),
            collators: r["c"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| collate::get_collator(c.as_str().unwrap()))
                .collect(),
            low_exclude: r["le"].as_bool().unwrap(),
            high_exclude: r["he"].as_bool().unwrap(),
        })
        .collect()
}

/// The part of Go's output a sub-result compares with.
fn want_of<'a>(want: &'a Value, key: &str) -> &'a Value {
    if key.is_empty() {
        return want;
    }
    if let Some(rest) = key.strip_prefix("idx")
        && let Some((i, k)) = rest.split_once('.')
        && let Ok(i) = i.parse::<usize>()
    {
        return &want["idx"][i][k];
    }
    &want[key]
}

fn replay(path: &str) -> BTreeMap<String, usize> {
    let mut gz = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap())
        .read_to_string(&mut gz)
        .unwrap();
    let mut lines = gz.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let mut n = 0usize;
    let mut passed: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked_at: BTreeMap<String, usize> = BTreeMap::new();
    let mut fails: Vec<String> = Vec::new();
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let p = rec["args"].as_array().unwrap();
        let want = &rec["out"];
        for (key, got) in exec(op, p, want) {
            let w = want_of(want, &key);
            match got {
                Got::Blocked(b) => {
                    let at = key.split('.').next_back().unwrap_or("");
                    *blocked_at.entry(format!("{op}.{at}:{b}")).or_default() += 1;
                    *blocked.entry(b).or_default() += 1
                }
                // A Go panic: tidb-rs must not return a different success (none expected).
                Got::Value(_) if w.get("panic").is_some() => {
                    *passed.entry(op.to_string()).or_default() += 1
                }
                Got::Value(g) if &g == w || wrap_equal(&g, w) => {
                    *passed.entry(op.to_string()).or_default() += 1
                }
                Got::Value(g) => {
                    // The differing fields of an object result.
                    let diff = match (&g, w) {
                        (Value::Object(a), Value::Object(b)) => {
                            let mut d = serde_json::Map::new();
                            for (k, v) in b {
                                if a.get(k) != Some(v) {
                                    d.insert(k.clone(), json!({ "go": v, "rust": a.get(k) }));
                                }
                            }
                            for (k, v) in a {
                                if !b.contains_key(k) {
                                    d.insert(k.clone(), json!({ "go": null, "rust": v }));
                                }
                            }
                            Value::Object(d)
                        }
                        _ => json!({ "go": w, "rust": g }),
                    };
                    fails.push(format!(
                        "{op} [{key}] {} src={}\n  diff: {diff}",
                        serde_json::to_string(&p[..p.len().min(5)]).unwrap(),
                        rec["src"]
                    ))
                }
            }
        }
    }
    eprintln!("{path}: {n} records, passed {passed:?}, blocked {blocked_at:?}");
    let shown: Vec<String> = fails
        .iter()
        .take(
            std::env::var("GOVEC_SHOW")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        )
        .map(|s| s.chars().take(4000).collect::<String>())
        .collect();
    assert!(
        fails.is_empty(),
        "{path}: {} results differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    // Only D08.9 is a boundary of this block (spec D10.4 §5).
    assert!(
        blocked.keys().all(|b| b == "D08.9"),
        "unexpected boundary {blocked:?}"
    );
    blocked_at
}

#[test]
fn govec_rangerbuild() {
    let blocked = replay(&vectors());
    let got: Vec<(&str, usize)> = blocked.iter().map(|(b, n)| (b.as_str(), *n)).collect();
    assert_eq!(
        got, EXPECTED_BLOCKED,
        "blocked results per block changed (pin EXPECTED_BLOCKED)"
    );
    if let Ok(files) = std::env::var("GOVEC_RANGERBUILD_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
