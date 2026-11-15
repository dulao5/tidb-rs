//! Replays the govec `fastplan` vectors, op `fast` (tidb-rs-tracker
//! `govec/domains/fastplan.go`): block D10.8, `TryFastPlan` on SELECT statements against Go
//! TiDB v8.5.8 — which statements become a point get, and the plan (handle / index values,
//! schema with the column UniqueIDs, output names, privilege inputs, plan ID, memory usage,
//! the statement's tables and warnings).
//!
//! Go preprocesses the statement against a mock info schema; the Rust preprocessor cannot
//! resolve base tables yet (D06.3 part 2), so the replay resolves the single table of the
//! FROM clause itself (Go's `TryFastPlan` only reads that one). Records Go could not parse or
//! preprocess are skipped; a statement Go parses must parse in Rust.
//!
//! Records whose Rust result is a boundary stub (`blocked-by:<block>`) are counted per block
//! and the counts are pinned (constitution §7.3), with the Go outcome of each; every other
//! record must equal Go's.
//!
//! The new-collation switch is process-wide: this file holds one test.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::new_static_warn_handler;
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_base::mysql::{SQLMode, ServerStatusAutocommit};
use tidb_model::{DBInfo, TableInfo};
use tidb_parser::ast::{Stmt, TableName};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::PhysicalPlan;
use tidb_planner::core::point_get_plan::{
    PointGetPlan, get_single_table_name_and_alias, try_fast_plan,
};
use tidb_planner::core::resolve::{TableNameW, new_node_w, table_name_key};
use tidb_planner::planctx::PlanCtx;
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::MyDecimal;
use tidb_types::collate;
use tidb_types::context::Flags;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::field_name::NameSlice;

/// The records answered by each boundary stub, and Go's outcome for them.
///
/// - D02.2: optimizer hints in comments are not in the Rust AST;
/// - D06.5: the latest-schema check of FOR UPDATE / READ COMMITTED with a connection (Go's
///   mock session has no domain and returns no plan);
/// - D10.1: `PhysicalTableDual`;
/// - D10.8: `WHERE ... IN (...)` that may become a `BatchPointGetPlan`;
/// - D10.9: the optimizer trace of the fast plan (`tidb_enable_optimize_trace`).
const EXPECTED_BLOCKED: &[(&str, &str, usize)] = &[
    ("D02.2", "Point_Get", 13),
    ("D02.2", "nil", 17),
    ("D06.5", "nil", 26),
    ("D10.1", "TableDual", 62),
    ("D10.8", "Batch_Point_Get", 11),
    ("D10.8", "nil", 3),
    ("D10.9", "Point_Get", 38),
];

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------- values (as govec_pointget.rs)

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// vec.GoString's JSON form.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
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

/// d2FtOut.
fn ft_out(ft: &FieldType) -> Value {
    let elems = ft
        .get_elems()
        .map(|e| Value::Array(e.iter().map(|s| gs(s)).collect()))
        .unwrap_or(Value::Null);
    json!({"tp": ft.get_type(), "flag": ft.get_flag(), "flen": ft.get_flen(), "dec": ft.get_decimal(),
        "cs": ft.get_charset(), "co": ft.get_collate(), "elems": elems})
}

/// The capacity encoding/json gives a slice of n decoded elements (grow by 1.5x from 4).
fn go_json_cap(n: usize) -> usize {
    let mut cap = 0usize;
    for i in 0..n {
        if i >= cap {
            cap += cap / 2;
            if cap < 4 {
                cap = 4;
            }
        }
    }
    cap
}

/// Go's MemoryUsage minus Rust's for one field type (see `plan_out`).
fn go_cap_adjust(ft: &FieldType) -> i64 {
    let mut d = 0i64;
    // The plan's field types are clones (`Vec::clone`: capacity == length).
    if let Some(e) = ft.get_elems() {
        d += (go_json_cap(e.len()) as i64 - e.len() as i64) * 16;
    }
    d
}

/// A float64 as Go's encoding/json writes it (integral values without a fraction).
fn num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        json!(f as i64)
    } else {
        json!(f)
    }
}

fn ci(s: &CIStr) -> Value {
    json!([s.o, s.l])
}

fn names_out(ns: &NameSlice) -> Value {
    Value::Array(
        ns.iter()
            .map(|n| {
                json!({"db": ci(&n.db_name), "otbl": ci(&n.orig_tbl_name), "tbl": ci(&n.tbl_name),
                    "ocol": ci(&n.orig_col_name), "col": ci(&n.col_name)})
            })
            .collect(),
    )
}

fn loc_of(v: &Value) -> Arc<Location> {
    if v.is_null() {
        utc()
    } else {
        fixed_zone("Z", v.as_i64().unwrap() as i32)
    }
}

/// tblCtx: [castCtx, loc]; castCtx is [typeFlags, [7]errLevels, maxAllowedPacket, sqlMode,
/// now (unix ns)].
fn expr_ctx_of(v: &Value) -> tidb_sessionctx::exprctx::ExprContextRef {
    let outer = v.as_array().unwrap();
    let a = outer[0].as_array().unwrap();
    let mut lm: LevelMap = Default::default();
    for (i, l) in a[1].as_array().unwrap().iter().enumerate() {
        lm[i] = Level(l.as_u64().unwrap() as _);
    }
    let ns = a[4].as_i64().unwrap();
    let opts = vec![
        es::with_type_flags(Flags(a[0].as_u64().unwrap() as u16)),
        es::with_err_level_map(lm),
        es::with_warn_handler(Some(Arc::new(new_static_warn_handler(0)))),
        es::with_max_allowed_packet(a[2].as_u64().unwrap()),
        es::with_sql_mode(SQLMode(a[3].as_u64().unwrap() as i64)),
        es::with_current_time(Arc::new(move || Ok(tidb_base::gostd::time::unix(0, ns)))),
        es::with_location(Some(loc_of(&outer[1]))),
    ];
    let ev = es::new_eval_context(opts);
    es::new_expr_context(vec![es::with_eval_ctx(ev)])
}

fn plan_out(p: &PointGetPlan, vars: &SessionVars) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!("Point_Get"));
    o.insert("id".into(), json!(p.id()));
    o.insert("db".into(), json!(p.db_name));
    o.insert("tbl".into(), json!(p.tbl_info.id));
    o.insert("unsigned".into(), json!(p.unsigned_handle));
    o.insert("handle_col_offset".into(), json!(p.handle_col_offset));
    o.insert("table_dual".into(), json!(p.is_table_dual));
    o.insert("lock".into(), json!(p.lock));
    o.insert("lock_wait".into(), json!(p.lock_wait_time));
    o.insert(
        "index_values".into(),
        Value::Array(p.index_values.iter().map(datum_out).collect()),
    );
    // FieldType.MemoryUsage counts cap(Elems): Go's encoding/json grows the slice (cap 4 for
    // up to 4 elements), the Rust Vec has its own capacity (D02.3, a documented difference).
    // The Rust figure is adjusted by the capacity difference of every field type counted.
    let mut fts: Vec<&FieldType> = p.schema.columns.iter().map(|c| &*c.ret_type).collect();
    fts.extend(p.cols_field_type.iter().map(|f| &**f));
    fts.extend(p.handle_field_type.iter().map(|f| &**f));
    let adjust: i64 = fts.iter().map(|f| go_cap_adjust(f)).sum();
    o.insert("mem".into(), json!(p.memory_usage() + adjust));
    o.insert(
        "rows".into(),
        num(p.stats_info().map_or(0.0, |s| s.row_count)),
    );
    o.insert("est_rows".into(), num(p.get_est_row_count_for_display()));
    o.insert("stats_count".into(), num(p.stats_count()));
    o.insert(
        "operator_info".into(),
        json!(p.operator_info(false, &vars.enable_redact_log)),
    );
    o.insert(
        "operator_info_norm".into(),
        json!(p.operator_info(true, &vars.enable_redact_log)),
    );
    o.insert(
        "plan_col_id".into(),
        json!(
            vars.state
                .plan_column_id
                .0
                .load(std::sync::atomic::Ordering::SeqCst)
        ),
    );
    if let Some(h) = &p.handle {
        o.insert("handle".into(), json!(h.int_value()));
    }
    if let Some(ft) = &p.handle_field_type {
        o.insert("handle_ft".into(), ft_out(ft));
    }
    if let Some(idx) = &p.index_info {
        o.insert("index".into(), json!(idx.name.l));
    }
    o.insert(
        "cols_ft".into(),
        Value::Array(p.cols_field_type.iter().map(|f| ft_out(f)).collect()),
    );
    o.insert(
        "partition_names".into(),
        Value::Array(p.partition_names.iter().map(|n| json!(n.l)).collect()),
    );
    if let Some(i) = p.partition_idx {
        o.insert("partition_idx".into(), json!(i));
    }
    o.insert(
        "schema".into(),
        Value::Array(
            p.schema
                .columns
                .iter()
                .map(|c| {
                    json!({"id": c.id, "uid": c.unique_id, "idx": c.index, "orig": c.orig_name,
                        "ft": ft_out(&c.ret_type)})
                })
                .collect(),
        ),
    );
    o.insert("names".into(), names_out(&p.output_names));
    o.insert("unfold".into(), json!(p.unfold_from_wild_card));
    o.insert("where_cols".into(), json!(p.cols_in_where_clause));
    o.insert(
        "stmt_tables".into(),
        Value::Array(
            vars.stmt_ctx()
                .tables
                .iter()
                .map(|t| json!([t.db, t.table]))
                .collect(),
        ),
    );
    Value::Object(o)
}

enum Outcome {
    Value(Value),
    Unparsed,
    Blocked(String),
}

fn run(rec: &Value) -> Outcome {
    let args = rec["args"].as_array().unwrap();
    let sess = &args[1];
    collate::set_new_collation_enabled_for_test(sess["nc"].as_bool().unwrap());
    let sql = args[3].as_str().unwrap();
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let tables: Vec<Arc<TableInfo>> = args[2]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut ti = TableInfo::default();
            ti.unmarshal_json(t.as_str().unwrap().as_bytes()).unwrap();
            Arc::new(ti)
        })
        .collect();

    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = sess["db"].as_str().unwrap().to_owned();
    if let Some(l) = sess.get("select_limit") {
        vars.select_limit = l.as_u64().unwrap();
    }
    vars.enable_stable_result_mode = sess.get("stable").is_some_and(|v| v.as_bool().unwrap());
    if let Some(f) = sess.get("fix52592") {
        vars.state
            .optimizer_fix_control
            .insert(52592, f.as_str().unwrap().to_owned());
    }
    vars.optimizer_use_invisible_indexes =
        sess.get("invisible").is_some_and(|v| v.as_bool().unwrap());
    vars.set_status_flag(
        ServerStatusAutocommit,
        !sess
            .get("no_autocommit")
            .is_some_and(|v| v.as_bool().unwrap()),
    );
    if sess.get("rc").is_some_and(|v| v.as_bool().unwrap()) {
        vars.state.txn_ctx.isolation = "READ-COMMITTED".to_owned();
    }
    vars.connection_id = sess.get("conn").map_or(0, |v| v.as_u64().unwrap());
    vars.stmt_ctx_mut().enable_optimize_trace =
        sess.get("trace").is_some_and(|v| v.as_bool().unwrap());

    // Resolve the single table of the FROM clause (Go's preprocessor; see the module doc):
    // the preprocessor also qualifies the name with the current database.
    if let Stmt::Select(sel) = &mut stmt
        && let Some(from) = sel.from.as_deref_mut()
        && from.table_refs.right.is_none()
        && let Some(tidb_parser::ast::ResultSetNode::TableSource(ts)) = &mut from.table_refs.left
        && let tidb_parser::ast::ResultSetNode::TableName(tn) = &mut ts.source
        && tn.schema.o.is_empty()
    {
        tn.schema = tidb_parser::ast::CIStr::new(vars.current_db.clone());
    }
    let mut resolved: Option<TableNameW> = None;
    if let Stmt::Select(sel) = &stmt
        && let (Some(tn), _) = get_single_table_name_and_alias(sel.from.as_deref())
    {
        let tn: &TableName = tn;
        let db = if tn.schema.o.is_empty() {
            vars.current_db.to_lowercase()
        } else {
            tn.schema.l()
        };
        if db == "test"
            && let Some(t) = tables.iter().find(|t| t.name.l == tn.name.l())
        {
            resolved = Some(TableNameW {
                table_name: table_name_key(tn),
                db_info: Some(Arc::new(DBInfo {
                    id: 1,
                    name: CIStr::new("test"),
                    ..DBInfo::default()
                })),
                table_info: Arc::clone(t),
            });
        }
    }
    let node = new_node_w(&mut stmt);
    if let Some(r) = resolved {
        node.get_resolve_context().lock().add_table_name(r);
    }
    let expr_ctx = expr_ctx_of(&args[0]);
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        try_fast_plan(&mut ctx, &node)
    }));
    drop(ctx);
    let p = match res {
        Err(_) => return Outcome::Value(json!({"p": {"panic": true}})),
        Ok(Err(e)) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(json!({"err": e.to_string()}));
        }
        Ok(Ok(None)) => Value::Null,
        Ok(Ok(Some(p))) => {
            let pg = p
                .as_any()
                .downcast_ref::<PointGetPlan>()
                .expect("a point get");
            plan_out(pg, &vars)
        }
    };
    let w: Vec<Value> = vars
        .stmt_ctx()
        .get_warnings()
        .iter()
        .map(|w| gs(&w.err.error()))
        .collect();
    Outcome::Value(json!({"p": p, "w": w}))
}

/// Go's outcome, for the blocked counts.
fn go_kind(out: &Value) -> String {
    match &out["p"] {
        Value::Null => "nil".to_owned(),
        p => p["tp"].as_str().unwrap_or("panic").to_owned(),
    }
}

#[test]
fn govec_fastplan() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/fastplan.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "fastplan");
    let (mut matched, mut point_gets, mut skipped) = (0, 0, 0);
    let (mut unparsed, mut fails) = (Vec::new(), Vec::new());
    let mut blocked: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if rec["op"] != "fast" {
            continue;
        }
        let out = &rec["out"];
        if out.get("pre").is_some() {
            // Go could not parse or preprocess it: nothing to compare.
            skipped += 1;
            continue;
        }
        let sql = rec["args"][3].as_str().unwrap().to_owned();
        match run(&rec) {
            Outcome::Unparsed => unparsed.push(sql),
            Outcome::Blocked(b) => blocked
                .entry((b, go_kind(out)))
                .or_default()
                .push(format!("{sql} {}", rec["args"][1])),
            Outcome::Value(got) if got == *out => {
                matched += 1;
                if !out["p"].is_null() {
                    point_gets += 1;
                }
            }
            Outcome::Value(got) => fails.push(format!("{line}\n    got {got}")),
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "fastplan: {matched} matched ({point_gets} point gets), {skipped} skipped (Go preprocess), blocked {:?}, {} not parsed",
        blocked
            .iter()
            .map(|(k, v)| (k.clone(), v.len()))
            .collect::<Vec<_>>(),
        unparsed.len()
    );
    assert!(
        fails.is_empty(),
        "{} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        unparsed.is_empty(),
        "Go parses, Rust does not: {unparsed:?}"
    );
    let counts: Vec<(&str, &str, usize)> = blocked
        .iter()
        .map(|((b, g), v)| (b.as_str(), g.as_str(), v.len()))
        .collect();
    assert_eq!(counts, EXPECTED_BLOCKED, "blocked records: {blocked:#?}");
}
