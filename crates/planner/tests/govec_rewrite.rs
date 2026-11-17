//! Replays the govec `rewrite` vectors (tidb-rs-tracker `govec/domains/rewrite.go`): the
//! planner's expression rewriter outside subqueries (D09.2 part 1) against Go TiDB v8.5.8.
//!
//! Each record is one expression, parsed as the only field of `select <expr> from <table>`,
//! rewritten with the table's columns as the schema (`rewriteAstExprWithPlanCtx`): the
//! resulting tree (every node's type, collation, coercibility; constants with their
//! parameter-marker order and deferred function), its string, the visit infos and the
//! warnings must equal Go's, and so must the error when Go fails.
//!
//! Records whose Rust result is a boundary stub (`blocked-by:<block>`, D09.2 spec §4) are
//! counted per block and the counts are pinned (constitution §7.3). Records Go cannot parse
//! are skipped; a statement Go parses must parse in Rust.
//!
//! The new-collation switch is process-wide: this file holds one test.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{
    StaticWarnHandler, WarnHandler, new_plan_cache_tracker, new_static_warn_handler,
};
use tidb_base::gostd::time::utc;
use tidb_base::mysql::SQLMode;
use tidb_expression::expression::{Expr, table_info_to_schema_and_names};
use tidb_model::TableInfo;
use tidb_parser::ast::Stmt;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::expression_rewriter::rewrite_ast_expr_with_plan_ctx;
use tidb_sessionctx::errctx::{self, Level, LevelMap};
use tidb_sessionctx::exprctx::BuildContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::collate;
use tidb_types::context::Flags;
use tidb_types::datum::{self as dm, Datum};

/// The records answered by each boundary stub (D09.2 spec §4).
///
/// - D02.2: AST nodes the Rust parser does not model;
/// - D09.2 (part 2): system variables; D08.9: user variables;
/// - D09.4: GROUPING(); D09.6: subqueries; D09.15: DEFAULT() and VALUES().
/// - D08.6 / D08.7 / D08.9: built-in function families whose classes are not ported yet
///   (LIKE, string functions; time functions; IN, CASE, IF / IFNULL / NULLIF, ROW, ...): the
///   rewriter builds the call, the registry stops it.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    ("D08.6", 407),
    ("D08.7", 20),
    ("D08.9", 473),
    ("D09.15", 4),
    ("D09.2", 4),
    ("D09.4", 2),
    ("D09.6", 12),
];

/// Records that differ from Go only by a Go aliasing effect that is not emulated (D09.2
/// spec §5 item 3): Go pushes the schema's `*Column` itself for every reference to a column,
/// so the coercibility one reference derives lazily is cached for all of them; an ENUM column
/// first compared as a string (implicit coercibility) and then cast to INT keeps "implicit"
/// on the cast's clone, where tidb-rs derives "numeric" from the INT-flagged type. The value
/// only matters for string collation derivation, which an INT context never does.
const KNOWN_ALIASING_DIFFS: &[&str] =
    &["((('10' = j) >= f) <= ((cast(h as signed)) div (j << true)))"];

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

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

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

/// d2DatumIn (the kinds the generator binds to parameters).
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
        dm::KindMysqlDecimal => d.set_mysql_decimal(dec_of(p[1].as_str().unwrap())),
        _ => panic!("datum_in: kind {k}"),
    }
    d
}

/// d2DatumInOf.
fn datum_dump(d: &Datum) -> Value {
    match d.kind() {
        dm::KindNull => json!([d.kind()]),
        dm::KindInt64 => json!([d.kind(), d.get_int64()]),
        dm::KindUint64 => json!([d.kind(), d.get_uint64()]),
        dm::KindFloat64 => json!([d.kind(), d.get_float64().to_bits()]),
        dm::KindString => json!([d.kind(), gs(d.get_string()), d.collation()]),
        dm::KindMysqlDecimal => json!([d.kind(), d.get_mysql_decimal().string()]),
        dm::KindBytes => json!([d.kind(), hex(d.get_bytes())]),
        dm::KindMysqlTime => json!([d.kind(), d.get_mysql_time().raw().0]),
        dm::KindMysqlDuration => {
            let x = d.get_mysql_duration();
            json!([d.kind(), x.duration, x.fsp])
        }
        dm::KindMysqlJSON => {
            let j = d.get_mysql_json();
            json!([d.kind(), j.type_code, hex(&j.value)])
        }
        dm::KindFloat32 => json!([d.kind(), f64::from(d.get_float32()).to_bits()]),
        dm::KindBinaryLiteral | dm::KindMysqlBit => json!([d.kind(), hex(d.get_bytes())]),
        dm::KindMysqlEnum => {
            let e = d.get_mysql_enum();
            json!([d.kind(), e.value, gs(&e.name), d.collation()])
        }
        dm::KindMysqlSet => {
            let st = d.get_mysql_set();
            json!([d.kind(), st.value, gs(&st.name), d.collation()])
        }
        k => panic!("datum_dump: kind {k}"),
    }
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

/// rwDump.
fn dump(e: &Expr) -> Value {
    match e {
        Expr::Column(c) => json!({ "col": [
            c.unique_id, c.id, c.index, ft_out(&c.ret_type), gs(c.orig_name.as_bytes()),
            e.coercibility().0, e.repertoire().0
        ] }),
        Expr::CorrelatedColumn(_) => json!({ "corr": "unexpected" }),
        Expr::Constant(c) => {
            let pm = c.param_marker.map_or(-1, |p| p.order);
            let def = c.deferred_expr.as_ref().map_or(Value::Null, |d| dump(d));
            json!({ "const": [
                datum_dump(&c.value), ft_out(&c.ret_type), pm, def, e.coercibility().0, e.repertoire().0
            ] })
        }
        Expr::ScalarFunction(x) => {
            let args: Vec<Value> = x.get_args().iter().map(|a| dump(a)).collect();
            let all_vec = x.get_args().iter().all(|a| a.vectorized());
            let sig_vec = if all_vec { x.vectorized() } else { true };
            let (chs, coll) = x.charset_and_collation();
            let ci = &x.function.collation_info;
            json!({ "f": [
                x.func_name.l, x.func_name.o, x.function.sig.name, ft_out(x.ret_type()), args,
                sig_vec, chs, coll, x.function.pb_code(), ci.has_coercibility(),
                x.coercibility().0, x.repertoire().0, ci.is_explicit_charset()
            ] })
        }
    }
}

enum Outcome {
    Unparsed,
    Blocked(String),
    Value(Value),
}

fn warns(h: &StaticWarnHandler) -> Value {
    Value::Array(
        h.copy_warnings(Vec::new())
            .iter()
            .map(|w| gs(&w.err.error()))
            .collect(),
    )
}

fn run(rec: &Value) -> Outcome {
    let args = rec["args"].as_array().unwrap();
    let mut ti = TableInfo::default();
    ti.unmarshal_json(args[0].as_str().unwrap().as_bytes())
        .unwrap();
    let expr_sql = args[1].as_str().unwrap();
    let params: Vec<Datum> = args[2].as_array().unwrap().iter().map(datum_in).collect();
    let o = &args[3];
    collate::set_new_collation_enabled_for_test(o["nc"].as_bool().unwrap());

    let sql = format!("select {expr_sql} from {}", ti.name.o);
    let Ok(out) = parse(&sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(Stmt::Select(sel)) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let Some(field) = sel.fields.fields.first().and_then(|f| f.expr.as_ref()) else {
        return Outcome::Unparsed;
    };

    let mut lm: LevelMap = Default::default();
    for (i, l) in o["levels"].as_array().unwrap().iter().enumerate() {
        lm[i] = Level(l.as_u64().unwrap() as _);
    }
    // StatementContext.SetTypeFlags / SetErrLevels (newErrCtx): the truncate group's level
    // follows the type flags.
    let flags = Flags(o["flags"].as_u64().unwrap() as u16);
    lm[errctx::ErrGroupTruncate.0 as usize] = if flags.ignore_truncate_err() {
        errctx::LevelIgnore
    } else if flags.truncate_as_warning() {
        errctx::LevelWarn
    } else {
        errctx::LevelError
    };
    let wh = Arc::new(new_static_warn_handler(0));
    let ev = es::new_eval_context(vec![
        es::with_type_flags(Flags(o["flags"].as_u64().unwrap() as u16)),
        es::with_err_level_map(lm),
        es::with_warn_handler(Some(Arc::clone(&wh) as _)),
        es::with_sql_mode(SQLMode(o["mode"].as_i64().unwrap())),
        es::with_location(Some(utc())),
        es::with_param_list(&params),
        es::with_current_db("test".to_owned()),
    ]);
    let tracker = Arc::new(new_plan_cache_tracker(Some(Arc::clone(&wh) as _)));
    if o["cache"].as_bool().unwrap() {
        tracker.enable_plan_cache();
    }
    let ctx = es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_plan_cache_tracker(tracker),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ]);
    let ctx_ref: BuildContextRef = Arc::clone(&ctx) as _;
    let (schema, names) =
        table_info_to_schema_and_names(&ctx_ref, &CIStr::new("test"), &ti).unwrap();
    let names: Vec<Arc<_>> = names.into_iter().map(Arc::new).collect();

    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rewrite_ast_expr_with_plan_ctx(&*ctx_ref, field, Some(&schema), &names, false)
    }));
    match res {
        Err(_) => Outcome::Value(json!({"panic": true})),
        Ok(Err(e)) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            Outcome::Value(json!({"err": gs(&e.error()), "w": warns(&wh)}))
        }
        Ok(Ok((e, vis))) => {
            let ectx = ctx_ref.get_eval_ctx();
            let s = e.string_with_ctx(&*ectx, tidb_base::errors::RedactLogDisable);
            let vi: Vec<Value> = vis
                .iter()
                .map(|v| {
                    json!([
                        v.privilege,
                        v.db,
                        v.table,
                        v.column,
                        gs(v.err.as_deref().unwrap_or("").as_bytes())
                    ])
                })
                .collect();
            Outcome::Value(json!({"e": dump(&e), "s": gs(&s), "vi": vi, "w": warns(&wh)}))
        }
    }
}

#[test]
fn govec_rewrite() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/rewrite.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "rewrite");
    let (mut matched, mut skipped, mut aliasing) = (0, 0, 0);
    let (mut unparsed, mut fails) = (Vec::new(), Vec::new());
    let mut blocked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let out = &rec["out"];
        if out.get("pre").is_some() {
            skipped += 1;
            continue;
        }
        let e = rec["args"][1].as_str().unwrap().to_owned();
        match run(&rec) {
            Outcome::Unparsed => unparsed.push(e),
            Outcome::Blocked(b) => blocked.entry(b).or_default().push(e),
            Outcome::Value(got) if got == *out => matched += 1,
            Outcome::Value(_) if KNOWN_ALIASING_DIFFS.contains(&e.as_str()) => aliasing += 1,
            Outcome::Value(got) => {
                if let Ok(dump) = std::env::var("GOVEC_REWRITE_DUMP") {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dump)
                        .unwrap();
                    writeln!(
                        f,
                        "{}",
                        json!({"e": e, "o": rec["args"][3], "want": out, "got": got})
                    )
                    .unwrap();
                }
                fails.push(format!(
                    "{e} {}\n    want {out}\n    got  {got}",
                    rec["args"][3]
                ))
            }
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "rewrite: {matched} matched, {aliasing} known aliasing differences, {skipped} skipped (Go parse), blocked {:?}, {} not parsed, {} differ",
        blocked
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect::<Vec<_>>(),
        unparsed.len(),
        fails.len()
    );
    assert!(
        fails.is_empty(),
        "{} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        unparsed.is_empty(),
        "Go parses, Rust does not: {unparsed:?}"
    );
    let counts: Vec<(&str, usize)> = blocked.iter().map(|(b, v)| (b.as_str(), v.len())).collect();
    assert_eq!(counts, EXPECTED_BLOCKED, "blocked records: {blocked:#?}");
}
