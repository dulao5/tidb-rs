//! Replays the govec `findbest` vectors (FindBestTask and the access paths of the first
//! vertical slice, specs/D10.2-find-best-task) against Go TiDB v8.5.8 (tracker
//! `govec/domains/findbest.go`, which documents the record format):
//!
//! - `plan`: a query goes through the whole `DoOptimize` (logical optimization with the
//!   index pruning of the statistics loading point, statistics derivation, FindBestTask,
//!   `postOptimize`) with pseudo statistics; the final physical plan, without row counts and
//!   costs (the plan columns of EXPLAIN without estRows / estCost), must equal Go's. Where Go
//!   picks an index reader (IndexReader / IndexLookUp, the part of FindBestTask outside the
//!   first slice) tidb-rs must stop at the index path boundary (`blocked-by:D10.2`);
//! - `prune`: `PruneIndexesByWhereAndOrder` over a DataSource's paths.
//!
//! The statements of the first vertical slice on sbtest1 (`sys/vs1`) must all plan without
//! an index reader in Go (sbtest1's secondary index `k_1` is pruned by the skyline pruning)
//! and must equal Go in tidb-rs.

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
use tidb_expression::{Expr, ExprRef};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::optimizer::{adjust_optimization_flags, do_optimize_plan};
use tidb_planner::core::physical_plans::{
    PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort, PhysicalTableDual,
    PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
};
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::planbuilder::new_plan_builder;
use tidb_planner::core::point_get_plan::PointGetPlan;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::core::rule::logical_rules::{
    FlagCollectPredicateColumnsPoint, FlagSyncWaitStatsLoadPoint,
};
use tidb_planner::core::rule::rule_prune_indexes::prune_indexes_by_where_and_order;
use tidb_planner::hint::hint_query_block::new_qb_hint_handler;
use tidb_planner::logicalop::DataSource;
use tidb_planner::util::{AccessPath, ByItems};
use tidb_planner::{LogicalKind, LogicalPlan, PhysicalKind, PhysicalPlan, PlanCtx, PlanId};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::collate;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The `plan` records whose Rust result is a boundary stub, per block, in the committed
/// vectors (the same on both targets; constitution §7.3). A boundary that starts answering,
/// or a new one, fails here: update the pin in the same PR. Set `GOVEC_FINDBEST_BLOCKED=1`
/// to list the statements. Every `D10.2` record is one where Go plans an index reader.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    // Comment-style hints (`/*+ USE_INDEX(...) */`): the parser keeps them opaque.
    ("D02.2", 2),
    // The preprocessor's missing table check (the privilege manager decides the error).
    ("D04.1", 2),
    // Built-in function families not ported yet (IN, ANY_VALUE, user variables, ...).
    ("D08.9", 40),
    // Virtual generated columns.
    ("D09.1", 2),
    // Partitioned tables.
    ("D09.12", 4),
    // BuildKeySolver in logicalOptimize (reached by aggregation / GROUP BY / DISTINCT queries).
    ("D09.13", 10),
    // The lock operator (SELECT ... FOR UPDATE).
    ("D09.14", 2),
    // System variables in expressions.
    ("D09.2", 2),
    // Joins.
    ("D09.3", 6),
    // Window functions (D09.4 part B).
    ("D09.4", 4),
    // WITH and set operations.
    ("D09.5", 4),
    // Subqueries.
    ("D09.6", 2),
    // An index path left after the skyline pruning where Go plans an index reader
    // (IndexReader / IndexLookUp, part 2 of FindBestTask).
    ("D10.2", 528),
    // An index path left after the skyline pruning where Go keeps the table path by cost:
    // the index reader's cost is part 2.
    ("D10.2 (Go: table)", 140),
    // `OR` conditions over two or more paths, or multi-valued indexes: index merge paths.
    ("D10.7", 60),
    // BatchPointGet (several point ranges on a unique key).
    ("D10.8", 7),
    // Index hints (USE / FORCE / IGNORE INDEX).
    ("D13.3", 2),
];

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/findbest.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

struct MockClient;

impl Client for MockClient {
    fn send(
        &self,
        _: &tidb_kv::Context,
        _: &Request,
        _: Option<tidb_kv::kv::OptionValue>,
        _: &ClientSendOption,
    ) -> Box<dyn Response> {
        unreachable!("MockClient.send")
    }

    fn is_request_type_supported(&self, req_type: i64, sub_type: i64) -> bool {
        RequestTypeSupportedChecker.is_request_type_supported(req_type, sub_type)
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
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

fn es_out(ctx: &PlanCtx<'_>, es: &[ExprRef]) -> Value {
    let ectx = ctx.eval_ctx();
    Value::Array(
        es.iter()
            .map(|e| gs(&e.string_with_ctx(&*ectx, RedactLogDisable)))
            .collect(),
    )
}

fn cols_out(ctx: &PlanCtx<'_>, cs: &[Arc<tidb_expression::Column>]) -> Value {
    let es: Vec<ExprRef> = cs
        .iter()
        .map(|c| Arc::new(Expr::Column(tidb_expression::Column::clone(c))))
        .collect();
    es_out(ctx, &es)
}

fn by_out(ctx: &PlanCtx<'_>, by: &[ByItems]) -> Value {
    let ectx = ctx.eval_ctx();
    Value::Array(
        by.iter()
            .map(|b| {
                json!([
                    gs(&b.expr.string_with_ctx(&*ectx, RedactLogDisable)),
                    b.desc
                ])
            })
            .collect(),
    )
}

/// The Go generator's fbDump.
fn dump(ctx: &PlanCtx<'_>, p: &dyn PhysicalPlan) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!(p.tp()));
    o.insert("id".into(), json!(p.id()));
    o.insert("schema".into(), cols_out(ctx, &p.schema().columns));
    let any = p.as_any();
    match p.kind() {
        PhysicalKind::TableReader => {
            let x = any.downcast_ref::<PhysicalTableReader>().unwrap();
            o.insert("store".into(), json!(x.store_type.name()));
            o.insert(
                "tbl".into(),
                dump(ctx, &**x.get_table_plan().expect("table plan")),
            );
        }
        PhysicalKind::TableScan => {
            let x = any.downcast_ref::<PhysicalTableScan>().unwrap();
            o.insert("table".into(), json!(x.table.name.o));
            o.insert(
                "ranges".into(),
                Value::Array(x.ranges.iter().map(|r| gs(&r.string())).collect()),
            );
            o.insert("access".into(), es_out(ctx, &x.access_condition));
            o.insert("keep".into(), json!(x.keep_order));
            o.insert("desc".into(), json!(x.desc));
        }
        PhysicalKind::PointGet => {
            let x = any.downcast_ref::<PointGetPlan>().unwrap();
            o.insert("table".into(), json!(x.tbl_info.name.o));
            if let Some(h) = &x.handle {
                o.insert("handle".into(), json!(h.int_value()));
            }
            if let Some(i) = &x.index_info {
                o.insert("index".into(), json!(i.name.o));
            }
            o.insert(
                "values".into(),
                Value::Array(
                    x.index_values
                        .iter()
                        .map(|d| gs(&d.to_string().unwrap_or_default()))
                        .collect(),
                ),
            );
            o.insert("access".into(), es_out(ctx, &x.access_conditions));
        }
        PhysicalKind::Selection => {
            let x = any.downcast_ref::<PhysicalSelection>().unwrap();
            o.insert("conds".into(), es_out(ctx, &x.conditions));
        }
        PhysicalKind::Projection => {
            let x = any.downcast_ref::<PhysicalProjection>().unwrap();
            o.insert("exprs".into(), es_out(ctx, &x.exprs));
        }
        PhysicalKind::Sort => {
            let x = any.downcast_ref::<PhysicalSort>().unwrap();
            o.insert("by".into(), by_out(ctx, &x.by_items));
        }
        PhysicalKind::TopN => {
            let x = any.downcast_ref::<PhysicalTopN>().unwrap();
            o.insert("by".into(), by_out(ctx, &x.by_items));
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        PhysicalKind::Limit => {
            let x = any.downcast_ref::<PhysicalLimit>().unwrap();
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        PhysicalKind::TableDual => {
            let x = any.downcast_ref::<PhysicalTableDual>().unwrap();
            o.insert("rows".into(), json!(x.row_count));
        }
        _ => {
            o.insert("other".into(), json!(true));
        }
    }
    let children: Vec<Value> = p.children().iter().map(|c| dump(ctx, &**c)).collect();
    o.insert("children".into(), Value::Array(children));
    Value::Object(o)
}

fn err_out(e: &Error) -> Value {
    json!({"err": gs(&e.error()), "code": e.code()})
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

// ---------------------------------------------------------------- plan

enum Outcome {
    Unparsed,
    Blocked(String),
    Value(Value),
}

fn new_expr_ctx(
    mode: i64,
    wh: &Arc<StaticWarnHandler>,
) -> tidb_sessionctx::exprctx::ExprContextRef {
    let ev = es::new_eval_context(vec![
        // StmtCtx's type context (stmtctx.NewStmtCtxWithTimeZone: types.DefaultStmtFlags).
        es::with_type_flags(tidb_types::context::DefaultStmtFlags),
        es::with_sql_mode(SQLMode(mode)),
        es::with_location(Some(utc())),
        es::with_current_db("test".to_owned()),
        es::with_warn_handler(Some(Arc::clone(wh) as _)),
    ]);
    es::new_expr_context(vec![
        es::with_eval_ctx(ev),
        es::with_charset("utf8mb4".to_owned(), "utf8mb4_bin".to_owned()),
        es::with_default_collation_for_utf8mb4("utf8mb4_bin".to_owned()),
    ])
}

/// The parsed, preprocessed and built query (the Go generator's fbBuild), then `f`.
fn with_built(
    tables: &Arc<Tables>,
    sql: &str,
    opts: &Value,
    f: &mut dyn FnMut(&mut PlanCtx<'_>, u64, PlanId) -> Result<Value, Error>,
) -> Outcome {
    collate::set_new_collation_enabled_for_test(true);
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
    vars.enable_stable_result_mode = opts["stable"].as_bool().unwrap();
    vars.stmt_ctx_mut().use_dynamic_prune_mode = opts["dynamic"].as_bool().unwrap();
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);

    let blocked_or = |e: Error| match blocked_by(&e) {
        Some(b) => Outcome::Blocked(b.to_string()),
        None => Outcome::Value(err_out(&e)),
    };
    let resolve_ctx = {
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
            return blocked_or(e);
        }
        node.get_resolve_context()
    };
    let mut hint_processor = new_qb_hint_handler();
    if let Err(e) = hint_processor.accept(&stmt, Some(vars.stmt_ctx())) {
        return blocked_or(e);
    }

    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), Some(Arc::new(hint_processor)));
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

    let wh = Arc::new(new_static_warn_handler(0));
    let mut ctx = PlanCtx::new(&mut vars, new_expr_ctx(mode, &wh));
    ctx.has_dirty_content = Some(Arc::new(|_| false));
    ctx.client = Some(Arc::new(MockClient));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let p = builder.build(&mut ctx, &stmt)?;
        let flag = adjust_optimization_flags(ctx.vars, builder.get_opt_flag());
        f(&mut ctx, flag, p)
    }));
    match res {
        Err(_) => Outcome::Value(json!({"panic": true})),
        Ok(Err(e)) => blocked_or(e),
        Ok(Ok(mut v)) => {
            if v.get("plan").is_some() {
                v["w"] = Value::Array(warns(ctx.vars, &wh));
                v["plan_id"] = json!(ctx.vars.state.plan_id.0.load(Ordering::SeqCst));
            }
            Outcome::Value(v)
        }
    }
}

fn run_plan(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    with_built(
        tables,
        args[1].as_str().unwrap(),
        &args[2],
        &mut |ctx, flag, p| {
            let run = flag & !(FlagCollectPredicateColumnsPoint | FlagSyncWaitStatsLoadPoint);
            let (plan, _) = do_optimize_plan(ctx, run, p)?;
            Ok(json!({ "plan": dump(ctx, &*plan) }))
        },
    )
}

fn first_data_source(ctx: &PlanCtx<'_>, p: PlanId) -> Option<PlanId> {
    let node = ctx.arena.get(p);
    if node.kind() == LogicalKind::DataSource {
        return Some(p);
    }
    node.children()
        .iter()
        .find_map(|&c| first_data_source(ctx, c))
}

fn path_name(p: &AccessPath) -> String {
    if p.is_table_path() {
        if p.store_type == tidb_base::kv::TiFlash {
            return "<table:tiflash>".to_owned();
        }
        return "<table>".to_owned();
    }
    p.index.as_ref().unwrap().name.o.clone()
}

fn run_prune(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    let opts = json!({"mode": 32, "keep_order": false, "stable": false, "dynamic": false});
    let cols: Vec<usize> = args[2]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as usize)
        .collect();
    let threshold = args[3].as_i64().unwrap() as isize;
    let fill = args[4].as_bool().unwrap();
    with_built(
        tables,
        args[1].as_str().unwrap(),
        &opts,
        &mut |ctx, _, p| {
            let Some(ds_id) = first_data_source(ctx, p) else {
                return Ok(json!({"unsupported": "no DataSource"}));
            };
            let ds = ctx.arena.downcast_ref::<DataSource>(ds_id).unwrap();
            let schema = ds.schema(&ctx.arena);
            let interesting: Vec<_> = cols
                .iter()
                .filter(|&&i| i < schema.len())
                .map(|&i| Arc::clone(&schema.columns[i]))
                .collect();
            let mut paths: Vec<AccessPath> = ds
                .all_possible_access_paths
                .iter()
                .map(AccessPath::clone_path)
                .collect();
            if fill {
                for path in &mut paths {
                    if path.is_table_path() || path.index.is_none() {
                        continue;
                    }
                    let index = Arc::clone(path.index.as_ref().unwrap());
                    (
                        path.idx_cols,
                        path.idx_col_lens,
                        path.full_idx_cols,
                        path.full_idx_col_lens,
                    ) = tidb_planner::util::column::index_info2_cols(
                        &ds.columns,
                        &schema.columns,
                        &index,
                    );
                }
            }
            let kept =
                prune_indexes_by_where_and_order(ds, ctx, &mut paths, &interesting, threshold);
            Ok(json!({
                "kept": kept.iter().map(|&i| path_name(&paths[i])).collect::<Vec<_>>(),
                "single": paths.iter().map(|p| p.is_single_scan).collect::<Vec<_>>(),
            }))
        },
    )
}

/// Whether a Go plan has an operator outside the slice (`other`: an index reader).
fn has_other(v: &Value) -> bool {
    v.get("other").is_some()
        || v.get("tbl").is_some_and(has_other)
        || v["children"]
            .as_array()
            .is_some_and(|cs| cs.iter().any(has_other))
}

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
fn govec_findbest() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "findbest");
    let mut tables: Option<Arc<Tables>> = None;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let (mut diffs, mut unparsed, mut vs1_index) = (Vec::new(), Vec::new(), Vec::new());
    let (mut vs1_records, mut vs1_equal) = (0usize, 0usize);
    let mut go_index = 0usize;
    let mut records = 0usize;
    let list = std::env::var("GOVEC_FINDBEST_BLOCKED").is_ok();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        records += 1;
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        let src = rec["src"].as_str().unwrap_or_default();
        assert!(rec["err"].is_null(), "Go error in {op} {}", args[1]);
        if op == "tables" {
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
        let tables = tables.as_ref().expect("tables first");
        let sql = args[1].as_str().unwrap();
        let go_other = out.get("plan").is_some_and(has_other);
        if src == "sys/vs1" {
            vs1_records += 1;
        }
        if go_other {
            go_index += 1;
            if src == "sys/vs1" {
                vs1_index.push(sql.to_owned());
            }
        }
        let outcome = match op {
            "plan" => run_plan(tables, args),
            "prune" => run_prune(tables, args),
            op => panic!("op {op}"),
        };
        let got = match outcome {
            Outcome::Unparsed => {
                if out["code"] != 1064 {
                    unparsed.push(sql.to_owned());
                }
                continue;
            }
            Outcome::Blocked(b) => {
                if list {
                    eprintln!("blocked {b} (go index reader: {go_other}): {sql}");
                }
                // An index path left after the skyline pruning is the boundary of this part
                // (VS1 plan §3.2): Go then compares the index readers' costs with the
                // table's. The records where Go keeps the table are counted apart.
                let key = if b == "D10.2" && !go_other && out.get("plan").is_some() {
                    "D10.2 (Go: table)".to_owned()
                } else {
                    b
                };
                *blocked.entry(key).or_default() += 1;
                continue;
            }
            Outcome::Value(v) => {
                if out.get("unsupported").is_some() {
                    diffs.push(format!(
                        "{sql}: Go plans {}, tidb-rs gives {v}",
                        out["unsupported"]
                    ));
                    continue;
                }
                v
            }
        };
        if &got == out {
            *counts.entry(op.to_owned()).or_default() += 1;
            if src == "sys/vs1" {
                vs1_equal += 1;
            }
        } else {
            diffs.push(format!(
                "{op} {sql}: {}",
                first_diff(out, &got, String::new()).unwrap_or_default()
            ));
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(records as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "findbest: {records} records, equal to Go {counts:?}, blocked {blocked:?} (Go plans an index reader in {go_index}), {} not parsed, {} differences",
        unparsed.len(),
        diffs.len()
    );
    for d in diffs.iter().take(30) {
        eprintln!("{d}");
    }
    assert!(
        vs1_index.is_empty(),
        "statements of the first slice on sbtest1 stop at an index reader in Go: {vs1_index:?}"
    );
    assert!(diffs.is_empty(), "{} differences", diffs.len());
    assert_eq!(
        vs1_equal, vs1_records,
        "every statement of the first slice on sbtest1 plans as in Go"
    );
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

// ---------------------------------------------------------------- the memo (taskMap)
//
// The `taskMap` spike (specs/VS1-vertical-slice/spike-taskmap.md) as tests of the real
// FindBestTask: a memo hit gives the same `Arc` and builds nothing (E2, the child shared by
// several parent candidates), the cost is cached on the shared node without copying it, the
// rollback of the `nth_plan` hint clears the entries after a time stamp (E3), and once the
// memo is dropped the best plan is owned alone: `ResolveIndices` and `postOptimize` copy no
// node (E1, and the end of `physicalOptimize`, N1 §3 item 9.3).

fn spike_tables() -> Arc<Tables> {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let rec: Value = serde_json::from_str(s.lines().nth(1).unwrap()).unwrap();
    assert_eq!(rec["op"], "tables");
    let ts = rec["out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let mut ti = TableInfo::default();
            ti.unmarshal_json(t.as_str().unwrap().as_bytes()).unwrap();
            Arc::new(ti)
        })
        .collect();
    Arc::new(Tables(ts))
}

fn spike_opts() -> Value {
    json!({"mode": 32, "keep_order": false, "stable": false, "dynamic": false})
}

/// Every node of the tree (and the table plans of the readers) is owned alone, the flat
/// list of a reader (`table_plans`) excepted (N1 §3 item 9.1).
fn owned_alone(p: &tidb_planner::PhysicalPlanRef) -> bool {
    let reader_extra = p
        .as_any()
        .downcast_ref::<PhysicalTableReader>()
        .is_none_or(|r| {
            r.get_table_plan().is_none_or(|tp| {
                // The chain node is also held by the flat list (Go's same pointers).
                Arc::strong_count(tp) <= 2 && owned_alone_chain(tp)
            })
        });
    Arc::strong_count(p) == 1 && reader_extra && p.children().iter().all(owned_alone)
}

fn owned_alone_chain(p: &tidb_planner::PhysicalPlanRef) -> bool {
    p.children()
        .iter()
        .all(|c| Arc::strong_count(c) <= 2 && owned_alone_chain(c))
}

#[test]
fn memo_hit_shares_the_task_without_copies() {
    let tables = spike_tables();
    let out = with_built(
        &tables,
        "SELECT c FROM sbtest1 WHERE id > 5",
        &spike_opts(),
        &mut |ctx, flag, p| {
            let p = tidb_planner::core::optimizer::logical_optimize(ctx, flag, p)?;
            tidb_planner::logicalop::base_logical_plan::recursive_derive_stats(ctx, p, &[])?;
            let ds = first_data_source(ctx, p).unwrap();
            let prop = tidb_planner::property::PhysicalProperty {
                task_tp: tidb_planner::property::RootTaskType,
                expected_cnt: f64::MAX,
                ..Default::default()
            };
            let opt = tidb_planner::util::optimizetrace::default_physical_optimize_option();
            let mut memo = tidb_planner::logicalop::base_logical_plan::TaskMap::new();
            let mut counter = tidb_sessionctx::planbase::PlanCounterTp(-1);
            let (t1, c1) =
                tidb_planner::base::find_best_task(ctx, ds, &mut memo, &prop, &mut counter, &opt)?;
            let plans_built = ctx.vars.state.plan_id.0.load(Ordering::SeqCst);
            let copies = tidb_planner::physicalop::clone_node_calls();
            // A second parent candidate asks the same child for the same property.
            let (t2, c2) =
                tidb_planner::base::find_best_task(ctx, ds, &mut memo, &prop, &mut counter, &opt)?;
            let same = Arc::ptr_eq(t1.plan().unwrap(), t2.plan().unwrap());
            // The cost of both candidates: cached on the shared node, which is not copied.
            let better =
                tidb_planner::core::find_best_task::compare_task_cost(ctx, &t2, &t1, &opt)?;
            let cached = t1
                .plan()
                .unwrap()
                .base_physical()
                .cost
                .lock()
                .plan_cost_init;
            Ok(json!({
                "counts": [c1, c2],
                "same": same,
                "new_plans": ctx.vars.state.plan_id.0.load(Ordering::SeqCst) - plans_built,
                "copies": tidb_planner::physicalop::clone_node_calls() - copies,
                "better": better,
                "cached": cached,
                "memo": memo.len(),
            }))
        },
    );
    let Outcome::Value(v) = out else {
        panic!("not planned")
    };
    assert_eq!(v["counts"], json!([1, 1]), "{v}");
    assert_eq!(v["same"], json!(true), "a memo hit is the same Arc: {v}");
    assert_eq!(v["new_plans"], json!(0), "a memo hit builds nothing: {v}");
    assert_eq!(
        v["copies"],
        json!(0),
        "the cost is cached on the shared node: {v}"
    );
    assert_eq!(v["better"], json!(false), "{v}");
    assert_eq!(v["cached"], json!(true), "{v}");
    assert_eq!(v["memo"], json!(1), "{v}");
}

#[test]
fn memo_rollback_clears_entries_after_the_time_stamp() {
    let tables = spike_tables();
    let out = with_built(
        &tables,
        "SELECT c FROM sbtest1 WHERE id > 5",
        &spike_opts(),
        &mut |ctx, flag, p| {
            use tidb_planner::logicalop::base_logical_plan::{
                TaskMap, get_logical_ts4_task_map, get_task, roll_back_task_map, store_task,
            };
            let p = tidb_planner::core::optimizer::logical_optimize(ctx, flag, p)?;
            tidb_planner::logicalop::base_logical_plan::recursive_derive_stats(ctx, p, &[])?;
            let ds = first_data_source(ctx, p).unwrap();
            // nth_plan(1): TaskMapNeedBackUp.
            ctx.vars.stmt_ctx_mut().stmt_hints.force_nth_plan = 1;
            ctx.vars.stmt_ctx_mut().task_map_bak_ts = 0;
            let root = tidb_planner::property::PhysicalProperty {
                task_tp: tidb_planner::property::RootTaskType,
                expected_cnt: f64::MAX,
                ..Default::default()
            };
            let cop = tidb_planner::property::PhysicalProperty {
                task_tp: tidb_planner::property::CopSingleReadTaskType,
                expected_cnt: f64::MAX,
                ..Default::default()
            };
            let opt = tidb_planner::util::optimizetrace::default_physical_optimize_option();
            let mut memo = TaskMap::new();
            let mut counter = tidb_sessionctx::planbase::PlanCounterTp(-1);
            let (t_root, _) =
                tidb_planner::base::find_best_task(ctx, ds, &mut memo, &root, &mut counter, &opt)?;
            let ts = get_logical_ts4_task_map(ctx);
            let (_, _) =
                tidb_planner::base::find_best_task(ctx, ds, &mut memo, &cop, &mut counter, &opt)?;
            let before = (
                get_task(&memo, ds, &root).is_some(),
                get_task(&memo, ds, &cop).is_some(),
            );
            roll_back_task_map(&mut memo, ctx, p, ts);
            let after = (
                get_task(&memo, ds, &root).is_some(),
                get_task(&memo, ds, &cop).is_some(),
            );
            // A new search after the rollback gives a new task; the old one stays valid.
            let mut counter = tidb_sessionctx::planbase::PlanCounterTp(-1);
            let (t_cop, _) =
                tidb_planner::base::find_best_task(ctx, ds, &mut memo, &cop, &mut counter, &opt)?;
            store_task(&mut memo, ctx, ds, &root, t_root.clone());
            Ok(json!({
                "before": [before.0, before.1],
                "after": [after.0, after.1],
                "again": get_task(&memo, ds, &cop).is_some() && !t_cop.invalid() || t_cop.invalid(),
                "ts": ctx.vars.stmt_ctx().task_map_bak_ts,
            }))
        },
    );
    let Outcome::Value(v) = out else {
        panic!("not planned")
    };
    assert_eq!(v["before"], json!([true, true]), "{v}");
    assert_eq!(
        v["after"],
        json!([true, false]),
        "the entry stored after ts is rolled back: {v}"
    );
    assert_eq!(v["again"], json!(true), "{v}");
}

#[test]
fn best_plan_is_owned_alone_after_the_memo() {
    let tables = spike_tables();
    for sql in [
        "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 100 ORDER BY c LIMIT 10",
        "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 100",
        "SELECT id + 1, pad FROM sbtest1 WHERE id >= 10 AND pad = 'a'",
        "SELECT c FROM sbtest1 WHERE id = 10 AND pad = 'x'",
    ] {
        let out = with_built(&tables, sql, &spike_opts(), &mut |ctx, flag, p| {
            let p = tidb_planner::core::optimizer::logical_optimize(ctx, flag, p)?;
            let mut counter = -1;
            let copies = tidb_planner::physicalop::clone_node_calls();
            let (plan, _) = tidb_planner::core::optimizer::physical_optimize(ctx, p, &mut counter)?;
            let physical_copies = tidb_planner::physicalop::clone_node_calls() - copies;
            let owned = owned_alone(&plan);
            let copies = tidb_planner::physicalop::clone_node_calls();
            let plan = tidb_planner::core::optimizer::post_optimize(ctx, plan)?;
            Ok(json!({
                "owned": owned,
                "physical_copies": physical_copies,
                "post_copies": tidb_planner::physicalop::clone_node_calls() - copies,
                "plan": dump(ctx, &*plan),
            }))
        });
        let Outcome::Value(v) = out else {
            panic!("not planned: {sql}")
        };
        assert_eq!(v["owned"], json!(true), "{sql}: {v}");
        assert_eq!(
            v["post_copies"],
            json!(0),
            "{sql}: postOptimize copies: {v}"
        );
        // The whole physical optimization, the `ResolveIndices` on the best plan at its end
        // included, copies no node: the memo and the best task are dropped before the plan
        // is written (a live memo would make the root shared and get it copied).
        assert_eq!(
            v["physical_copies"],
            json!(0),
            "{sql}: physicalOptimize copies: {v}"
        );
    }
}

/// `tidb_cost_model_version = 1`: the cost model v1 is not ported (exception B5); a SELECT
/// the new stack would plan stops at the cost model boundary in FindBestTask's cost
/// comparison, so the router falls back to the prototype (router test
/// `select_with_cost_model_v1_falls_back_to_the_prototype`).
#[test]
fn select_with_cost_model_v1_is_the_cost_model_boundary() {
    let tables = spike_tables();
    let out = with_built(
        &tables,
        "SELECT c FROM sbtest1 WHERE id BETWEEN 1 AND 100",
        &spike_opts(),
        &mut |ctx, flag, p| {
            ctx.vars.cost_model_version = 1;
            let (plan, _) = do_optimize_plan(ctx, flag, p)?;
            Ok(json!({ "plan": dump(ctx, &*plan) }))
        },
    );
    match out {
        Outcome::Blocked(b) => assert_eq!(b, "EXC.B"),
        _ => panic!("cost model v1 planned"),
    }
}
