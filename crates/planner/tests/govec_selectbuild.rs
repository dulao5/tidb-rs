//! Replays the govec `selectbuild` vectors (the SELECT builder of the first vertical slice:
//! `buildSelect` and the data source, selection, projection, ORDER BY and LIMIT it builds,
//! D09.1 / D09.4, specs/D09.1-select-builder) against Go TiDB v8.5.8 (tracker
//! `govec/domains/selectbuild.go`, which documents the record format).
//!
//! Go builds each query into a logical plan (`PlanBuilder.Build` after the preprocessor) and
//! records the plan (operators, schemas, output names, expressions with their types, the
//! data source's access paths), the plan and column id counters, the optimization flags,
//! the visit infos and the warnings, or the error. This replay parses the same SQL with the
//! Rust parser, preprocesses it against the same tables (`tables` record: Go's TableInfo
//! JSON) and builds it with the ported builder. A record whose Rust result is a boundary
//! (`blocked-by:<block>`) is counted per block and the counts are pinned
//! (`EXPECTED_BLOCKED`); every other record must equal Go's.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{StaticWarnHandler, WarnHandler, new_static_warn_handler};
use tidb_base::errors::RedactLogDisable;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::SQLMode;
use tidb_expression::{Column, Expr};
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::planbuilder::new_plan_builder;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::logicalop::{
    DataSource, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual,
};
use tidb_planner::util::{AccessPath, ByItems, HandleCols};
use tidb_planner::{LogicalKind, PlanCtx, PlanId};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::collate;
use tidb_types::field_name::NameSlice;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The records whose Rust result is a boundary stub, per block, in the committed vectors
/// (the same on both targets; constitution §7.3). A boundary that starts answering, or a new
/// one, fails here: update the pin in the same PR. See D09.1 spec §6 for what each block
/// stands for. Set `GOVEC_SELECTBUILD_BLOCKED=1` to list the statements (except the built-in
/// function families of D08.9).
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    // Comment-style optimizer hints (`/*+ ... */`): the Rust parser does not parse the hint
    // grammar, so the hint skeleton stops at a non-empty hint list.
    ("D02.2", 2),
    // The preprocessor's missing table check (the privilege manager decides the error).
    ("D04.1", 2),
    // Built-in function families not ported yet (IN, ANY_VALUE, user variables, ...).
    ("D08.9", 53),
    // Virtual generated columns.
    ("D09.1", 2),
    // Partitioned tables.
    ("D09.12", 4),
    // The lock operator (SELECT ... FOR UPDATE).
    ("D09.14", 2),
    // System variables in expressions (the rewriter's part 2).
    ("D09.2", 2),
    // Joins.
    ("D09.3", 6),
    // Aggregation, GROUP BY, DISTINCT, windows.
    ("D09.4", 15),
    // WITH and set operations.
    ("D09.5", 4),
    // Subqueries.
    ("D09.6", 2),
    // Index hints (USE / FORCE / IGNORE INDEX).
    ("D13.3", 2),
];

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/selectbuild.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- values

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

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

// ---------------------------------------------------------------- session

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

/// Go's `infoschema.MockInfoSchema(tables)`: database `test` (id 1) with the tables.
struct Tables(Vec<Arc<TableInfo>>);

impl InfoSchemaLookup for Tables {
    fn table_by_name(&self, schema: &CIStr, table: &CIStr) -> Result<Arc<TableInfo>, Error> {
        if schema.l == "test"
            && let Some(t) = self.0.iter().find(|t| t.name.l == table.l)
        {
            return Ok(Arc::clone(t));
        }
        Err(
            tidb_infoschema::error::ErrTableNotExists.fast_gen_by_args(vec![
                tidb_base::gofmt::Arg::from(schema.o.clone()),
                tidb_base::gofmt::Arg::from(table.o.clone()),
            ]),
        )
    }
    fn table_by_id(&self, id: i64) -> Result<Option<Arc<TableInfo>>, Error> {
        Ok(self.0.iter().find(|t| t.id == id).cloned())
    }
    fn schema_name_by_id(&self, id: i64) -> Option<CIStr> {
        (id == 1).then(|| CIStr::new("test"))
    }
    fn schema_meta_version(&self) -> i64 {
        0
    }
    fn snapshot(&self) -> Arc<dyn InfoSchemaLookup> {
        Arc::new(Tables(self.0.clone()))
    }
}

// ---------------------------------------------------------------- plan output

fn expr_out(ectx: &dyn tidb_sessionctx::exprctx::EvalContext, e: &Expr) -> Value {
    match e {
        Expr::Column(c) => json!([
            "c",
            c.unique_id,
            c.id,
            c.index,
            ft_out(&c.ret_type),
            gs(c.orig_name.as_bytes()),
            c.is_hidden
        ]),
        Expr::CorrelatedColumn(c) => json!(["r", c.column.unique_id, ft_out(&c.column.ret_type)]),
        Expr::Constant(k) => json!([
            "k",
            gs(&k.string_with_ctx(ectx, RedactLogDisable)),
            ft_out(&k.ret_type)
        ]),
        Expr::ScalarFunction(f) => {
            let args: Vec<Value> = f.get_args().iter().map(|a| expr_out(ectx, a)).collect();
            let (chs, coll) = f.charset_and_collation();
            json!([
                "f",
                f.func_name.l,
                f.function.sig.name,
                ft_out(f.ret_type()),
                args,
                chs,
                coll,
                e.coercibility().0,
                e.repertoire().0
            ])
        }
    }
}

fn exprs_out(
    ectx: &dyn tidb_sessionctx::exprctx::EvalContext,
    es: &[tidb_expression::ExprRef],
) -> Value {
    Value::Array(es.iter().map(|e| expr_out(ectx, e)).collect())
}

fn cols_out(ectx: &dyn tidb_sessionctx::exprctx::EvalContext, cs: &[Arc<Column>]) -> Value {
    Value::Array(
        cs.iter()
            .map(|c| expr_out(ectx, &Expr::Column(Column::clone(c))))
            .collect(),
    )
}

fn names_out(names: &NameSlice) -> Value {
    Value::Array(
        names
            .iter()
            .map(|n| {
                json!([
                    gs(n.db_name.o.as_bytes()),
                    gs(n.tbl_name.o.as_bytes()),
                    gs(n.col_name.o.as_bytes()),
                    gs(n.orig_tbl_name.o.as_bytes()),
                    gs(n.orig_col_name.o.as_bytes()),
                    n.hidden,
                    n.not_explicit_usable,
                    n.redundant
                ])
            })
            .collect(),
    )
}

fn handle_out(ectx: &dyn tidb_sessionctx::exprctx::EvalContext, h: &Option<HandleCols>) -> Value {
    match h {
        None => Value::Null,
        Some(h) if h.is_int() => {
            json!({"int": expr_out(ectx, &Expr::Column(Column::clone(h.get_col(0).unwrap())))})
        }
        Some(h) => {
            let cols: Vec<Value> = (0..h.num_cols())
                .map(|i| expr_out(ectx, &Expr::Column(Column::clone(h.get_col(i).unwrap()))))
                .collect();
            json!({ "common": cols })
        }
    }
}

fn paths_out(paths: &[AccessPath]) -> Value {
    Value::Array(
        paths
            .iter()
            .map(|p| {
                let name = p.index.as_ref().map_or(String::new(), |i| i.name.o.clone());
                let cols: Vec<Value> = p
                    .full_idx_cols
                    .iter()
                    .map(|c| c.as_ref().map_or(json!(-1), |c| json!(c.unique_id)))
                    .collect();
                json!([
                    name,
                    p.store_type.0,
                    p.is_int_handle_path,
                    p.is_common_handle_path,
                    cols,
                    p.full_idx_col_lens,
                    p.is_uk_shard_index_path,
                    p.forced,
                    p.index_look_up_push_down_by.0
                ])
            })
            .collect(),
    )
}

fn by_out(ectx: &dyn tidb_sessionctx::exprctx::EvalContext, by: &[ByItems]) -> Value {
    Value::Array(
        by.iter()
            .map(|b| json!([expr_out(ectx, &b.expr), b.desc]))
            .collect(),
    )
}

fn dump(ctx: &PlanCtx<'_>, p: PlanId) -> Value {
    let ectx = ctx.eval_ctx();
    let ectx = &*ectx;
    let node = ctx.arena.get(p);
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!(node.tp()));
    o.insert("id".into(), json!(node.id()));
    o.insert("qb".into(), json!(node.query_block_offset()));
    o.insert(
        "schema".into(),
        cols_out(ectx, &node.schema(&ctx.arena).columns),
    );
    o.insert("names".into(), names_out(node.output_names(&ctx.arena)));
    match node.kind() {
        LogicalKind::DataSource => {
            let x = node.as_any().downcast_ref::<DataSource>().unwrap();
            o.insert("tbl".into(), json!([x.table_info.id, x.table_info.name.o]));
            o.insert("db".into(), json!(x.db_name.o));
            o.insert(
                "as".into(),
                x.table_as_name.as_ref().map_or(Value::Null, |a| json!(a.o)),
            );
            o.insert("phys".into(), json!(x.physical_table_id));
            o.insert(
                "cols".into(),
                Value::Array(x.columns.iter().map(|c| json!(c.id)).collect()),
            );
            o.insert("handle".into(), handle_out(ectx, &x.handle_cols));
            o.insert("unmut".into(), handle_out(ectx, &x.un_mutable_handle_cols));
            o.insert("tbl_cols".into(), cols_out(ectx, &x.tbl_cols));
            o.insert("pushed".into(), exprs_out(ectx, &x.pushed_down_conds));
            o.insert("all".into(), exprs_out(ectx, &x.all_conds));
            o.insert(
                "common".into(),
                Value::Array(
                    x.common_handle_cols
                        .iter()
                        .map(|c| json!(c.unique_id))
                        .collect(),
                ),
            );
            o.insert("common_lens".into(), json!(x.common_handle_lens));
            o.insert("paths".into(), paths_out(&x.possible_access_paths));
            o.insert("all_paths".into(), paths_out(&x.all_possible_access_paths));
            o.insert("fu".into(), json!(x.is_for_update_read));
            o.insert("pst".into(), json!(x.prefer_store_type));
            o.insert("uk".into(), json!(x.contain_expr_prefix_uk));
        }
        LogicalKind::Selection => {
            let x = node.as_any().downcast_ref::<LogicalSelection>().unwrap();
            o.insert("conds".into(), exprs_out(ectx, &x.conditions));
        }
        LogicalKind::Projection => {
            let x = node.as_any().downcast_ref::<LogicalProjection>().unwrap();
            o.insert("exprs".into(), exprs_out(ectx, &x.exprs));
            o.insert("nodelay".into(), json!(x.calculate_no_delay));
        }
        LogicalKind::Limit => {
            let x = node.as_any().downcast_ref::<LogicalLimit>().unwrap();
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
            o.insert("cop".into(), json!(x.prefer_limit_to_cop));
        }
        LogicalKind::Sort => {
            let x = node.as_any().downcast_ref::<LogicalSort>().unwrap();
            o.insert("by".into(), by_out(ectx, &x.by_items));
        }
        LogicalKind::TableDual => {
            let x = node.as_any().downcast_ref::<LogicalTableDual>().unwrap();
            o.insert("rows".into(), json!(x.row_count));
        }
        k => panic!("dump {k:?}"),
    }
    let children: Vec<Value> = node.children().iter().map(|&c| dump(ctx, c)).collect();
    o.insert("children".into(), Value::Array(children));
    Value::Object(o)
}

fn err_out(e: &Error) -> Value {
    json!({"err": gs(&e.error()), "code": e.code()})
}

// ---------------------------------------------------------------- replay

enum Outcome {
    Unparsed,
    Blocked(String),
    Value(Value),
}

fn warns(vars: &SessionVars, h: &StaticWarnHandler) -> Vec<Value> {
    let mut w: Vec<Value> = vars
        .stmt_ctx()
        .get_warnings()
        .iter()
        .map(|w| gs(&w.err.error()))
        .collect();
    w.extend(
        h.copy_warnings(Vec::new())
            .iter()
            .map(|w| gs(&w.err.error())),
    );
    w
}

fn run(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    collate::set_new_collation_enabled_for_test(true);
    let sql = args[1].as_str().unwrap();
    let opts = &args[2];
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let mode = opts["mode"].as_i64().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.sql_mode = mode;
    vars.remove_orderby_in_subquery = !opts["keep_order"].as_bool().unwrap();
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);

    {
        let mut node = new_node_w(&mut stmt);
        let (_, res) = preprocess(
            &mut vars,
            &mut node,
            vec![with_preprocessor_return(PreprocessorReturn {
                info_schema: Some(is.clone()),
                ..Default::default()
            })],
        );
        if let Err(e) = res {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(err_out(&e));
        }
    }

    // BuildLogicalPlanForTest: plan and column ids from 0.
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;

    let wh = Arc::new(new_static_warn_handler(0));
    let ev = es::new_eval_context(vec![
        // StmtCtx's type context (stmtctx.NewStmtCtxWithTimeZone: types.DefaultStmtFlags).
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(mode)),
        es::with_location(Some(utc())),
        es::with_current_db("test".to_owned()),
        es::with_warn_handler(Some(Arc::clone(&wh) as _)),
    ]);
    let expr_ctx = es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ]);
    let mut ctx = PlanCtx::new(&mut vars, expr_ctx);
    ctx.has_dirty_content = Some(Arc::new(|_| false));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        builder.build(&mut ctx, &stmt)
    }));
    match res {
        Err(_) => Outcome::Value(json!({"panic": true})),
        Ok(Err(e)) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            Outcome::Value(err_out(&e))
        }
        Ok(Ok(p)) => {
            let plan = dump(&ctx, p);
            let plan_id = ctx.vars.state.plan_id.0.load(Ordering::SeqCst);
            let col_id = ctx.vars.state.plan_column_id.0.load(Ordering::SeqCst);
            let vi: Vec<Value> = builder
                .get_visit_info()
                .iter()
                .map(|v| {
                    let (msg, code) = v
                        .err
                        .as_ref()
                        .map_or((Vec::new(), 0), |e| (e.error(), e.code()));
                    json!([v.privilege, v.db, v.table, v.column, gs(&msg), code])
                })
                .collect();
            let w = warns(ctx.vars, &wh);
            Outcome::Value(json!({
                "plan": plan,
                "plan_id": plan_id,
                "col_id": col_id,
                "opt_flag": builder.get_opt_flag(),
                "vi": vi,
                "w": w,
            }))
        }
    }
}

/// The first path where two JSON values differ, with both sides (for the report).
fn first_diff(a: &Value, b: &Value, path: String) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for (k, v) in x {
                match y.get(k) {
                    Some(w) => {
                        if let Some(d) = first_diff(v, w, format!("{path}.{k}")) {
                            return Some(d);
                        }
                    }
                    None => return Some(format!("{path}.{k}: missing in rust")),
                }
            }
            y.keys()
                .find(|k| !x.contains_key(*k))
                .map(|k| format!("{path}.{k}: extra in rust"))
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (v, w))| first_diff(v, w, format!("{path}[{i}]"))),
        _ if a == b => None,
        _ => Some(format!("{path}:\n    go:   {a}\n    rust: {b}")),
    }
}

#[test]
fn govec_selectbuild() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "selectbuild");
    let mut tables: Option<Arc<Tables>> = None;
    let (mut records, mut equal, mut errors_equal) = (0usize, 0usize, 0usize);
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let (mut diffs, mut unparsed) = (Vec::new(), Vec::new());
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        records += 1;
        let args = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        if rec["op"] == "tables" {
            let ts = out
                .as_array()
                .unwrap()
                .iter()
                .map(|t| {
                    let mut ti = TableInfo::default();
                    ti.unmarshal_json(t.as_str().unwrap().as_bytes()).unwrap();
                    Arc::new(ti)
                })
                .collect();
            tables = Some(Arc::new(Tables(ts)));
            continue;
        }
        let sql = args[1].as_str().unwrap();
        match run(
            tables.as_ref().expect("the tables record comes first"),
            args,
        ) {
            Outcome::Unparsed => {
                // A syntax error on both sides is the same outcome.
                if out["code"] != 1064 {
                    unparsed.push(sql.to_owned());
                }
            }
            Outcome::Blocked(b) => {
                if std::env::var("GOVEC_SELECTBUILD_BLOCKED").is_ok() && b != "D08.9" {
                    eprintln!("blocked {b}: {sql}");
                }
                *blocked.entry(b).or_default() += 1
            }
            Outcome::Value(got) => {
                if out.get("unsupported").is_some() {
                    diffs.push(format!(
                        "{sql} {}: Go builds {}, tidb-rs gives {got}",
                        args[2], out["unsupported"]
                    ));
                } else if &got == out {
                    if out.get("err").is_some() {
                        errors_equal += 1;
                    } else {
                        equal += 1;
                    }
                } else {
                    diffs.push(format!(
                        "{sql} {}: {}",
                        args[2],
                        first_diff(out, &got, String::new()).unwrap_or_default()
                    ));
                }
            }
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(records as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "selectbuild: {records} records, {equal} plans and {errors_equal} errors equal to Go, blocked {blocked:?}, {} not parsed, {} differences",
        unparsed.len(),
        diffs.len()
    );
    for d in diffs.iter().take(20) {
        eprintln!("{d}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
    assert!(
        unparsed.is_empty(),
        "Go parses, Rust does not: {unparsed:?}"
    );
    let pinned: BTreeMap<String, usize> = EXPECTED_BLOCKED
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    assert_eq!(blocked, pinned, "boundary counts changed");
}
