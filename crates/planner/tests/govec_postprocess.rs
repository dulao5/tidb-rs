//! Replays the govec `postprocess` vectors (the post-processing of a physical plan of the
//! first vertical slice, specs/D10.9-postprocess) against Go TiDB v8.5.8 (tracker
//! `govec/domains/postprocess.go`, which documents the record format):
//!
//! - `plan`: a query is planned as in the `findbest` replay (`DoOptimize`, which ends with
//!   `ResolveIndices`); the resolved column indexes of every operator, the coprocessor DAG of
//!   every table reader (`ToPB` for TiKV, node by node, and for TiFlash, as protobuf bytes)
//!   and the access objects (`String`, `NormalizedString`, `SetIntoPB`) must equal Go's.
//!   Where Go plans an operator outside the slice tidb-rs stops at its boundary before
//!   post-processing (index readers, partitioned tables, virtual generated columns, ...);
//!   the boundaries are pinned in [`EXPECTED_BLOCKED`].
//! - `missing`: a column reference no child has, added to one operator; that operator's
//!   `ResolveIndices` must fail with Go's error.
//! - `access`: access objects built from their fields.
//!
//! The schema columns of a table reader are not compared: Go shares them with the table plan
//! (whose `ResolveIndicesItself` writes their `Index`), tidb-rs copies them (D10.9 spec §5).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{StaticWarnHandler, new_static_warn_handler};
use tidb_base::gostd::time::utc;
use tidb_base::kv::{TiFlash, TiKV};
use tidb_base::mysql::SQLMode;
use tidb_expression::{Column, Expr, ExprRef, Schema, new_schema};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::base::AccessObject;
use tidb_planner::core::access_object::{
    DynamicPartitionAccessObject, DynamicPartitionAccessObjects, IndexAccess, OtherAccessObject,
    ScanAccessObject,
};
use tidb_planner::core::optimizer::{adjust_optimization_flags, do_optimize_plan};
use tidb_planner::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
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
use tidb_planner::hint::hint_query_block::new_qb_hint_handler;
use tidb_planner::property::SortItem;
use tidb_planner::util::ByItems;
use tidb_planner::{PhysicalKind, PhysicalPlan, PhysicalPlanRef, PlanCtx, PlanId, make_mut_plan};
use tidb_sessionctx::exprctx::BuildContextRef;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::planctx::BuildPBContext;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::collate;
use tidb_types::field_type::new_field_type;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The `plan` / `missing` records whose Rust result is a boundary stub, per block, in the
/// committed vectors (the same on both targets; constitution §7.3). A boundary that starts
/// answering, or a new one, fails here: update the pin in the same PR. Set
/// `GOVEC_POSTPROCESS_BLOCKED=1` to list the statements.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    // String functions (LENGTH, LIKE).
    ("missing D08.6", 12),
    // Virtual generated columns (table tg).
    ("missing D09.1", 18),
    // Partitioned tables (table tp).
    ("missing D09.12", 18),
    // An index path left after the skyline pruning (sbtest1's k_1 covering `id`, tc's
    // secondary indexes).
    ("missing D10.2", 12),
    // BatchPointGet (`id = 3 OR id = 9`).
    ("missing D10.8", 6),
    ("plan D08.6", 4),
    ("plan D09.1", 6),
    ("plan D09.12", 6),
    ("plan D10.2", 11),
    ("plan D10.8", 2),
];

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/postprocess.jsonl.gz",
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

// ---------------------------------------------------------------- session

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

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

// ---------------------------------------------------------------- plan

fn err_out(e: &Error) -> Value {
    json!({"err": gs(&e.error()), "code": e.code()})
}

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
            return blocked_or(e);
        }
    }
    let mut hint_processor = new_qb_hint_handler();
    if let Err(e) = hint_processor.accept(&stmt, Some(vars.stmt_ctx())) {
        return blocked_or(e);
    }

    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), Some(Arc::new(hint_processor)));
    builder.sql_text = sql;

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
        Ok(Ok(v)) => Outcome::Value(v),
    }
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

// ---------------------------------------------------------------- dumps

/// The Go generator's ppIdx: the `Index` of the column references in pre-order.
fn idx(e: &Expr) -> Value {
    fn walk(e: &Expr, out: &mut Vec<Value>) {
        match e {
            Expr::Column(c) => out.push(json!(c.index)),
            Expr::CorrelatedColumn(_) => out.push(json!("corr")),
            Expr::ScalarFunction(f) => {
                for a in f.get_args() {
                    walk(a, out);
                }
            }
            Expr::Constant(_) => {}
        }
    }
    let mut out = Vec::new();
    walk(e, &mut out);
    Value::Array(out)
}

fn idxs(es: &[ExprRef]) -> Value {
    Value::Array(es.iter().map(|e| idx(e)).collect())
}

fn by(by: &[ByItems]) -> Value {
    Value::Array(by.iter().map(|b| idx(&b.expr)).collect())
}

fn cols(s: &Schema) -> Value {
    Value::Array(s.columns.iter().map(|c| json!(c.index)).collect())
}

fn virt(s: &Schema) -> Value {
    Value::Array(
        s.columns
            .iter()
            .filter_map(|c| c.virtual_expr.as_ref().map(|e| idx(e)))
            .collect(),
    )
}

fn part(items: &[SortItem]) -> Value {
    Value::Array(items.iter().map(|it| json!(it.col.index)).collect())
}

fn prefix(c: &Option<Arc<Column>>) -> Value {
    match c {
        None => Value::Null,
        Some(c) => json!(c.index),
    }
}

fn pb(r: Result<tipb::Executor, Error>) -> Result<Value, Error> {
    match r {
        Err(e) => {
            if blocked_by(&e).is_some() {
                return Err(e);
            }
            Ok(json!({"err": gs(&e.error())}))
        }
        Ok(e) => Ok(json!(hex(&e.write_to_bytes().unwrap()))),
    }
}

fn acc(a: &dyn AccessObject) -> Value {
    let mut pb = tipb::ExplainOperator::default();
    a.set_into_pb(Some(&mut pb));
    a.set_into_pb(None);
    json!({
        "s": gs(a.to_string().as_bytes()),
        "n": gs(a.normalized_string().as_bytes()),
        "pb": hex(&pb.write_to_bytes().unwrap()),
    })
}

fn has_point_get(p: &dyn PhysicalPlan) -> bool {
    p.kind() == PhysicalKind::PointGet || p.children().iter().any(|c| has_point_get(&**c))
}

fn build_pb_ctx(ctx: &PlanCtx<'_>) -> BuildPBContext {
    let expr_ctx: BuildContextRef = ctx.expr_ctx.clone();
    BuildPBContext {
        expr_ctx,
        client: Arc::new(MockClient),
        tiflash_fast_scan: ctx.vars.tiflash_fast_scan,
        tiflash_fine_grained_shuffle_batch_size: ctx.vars.tiflash_fine_grained_shuffle_batch_size,
        group_concat_max_len: ctx.vars.group_concat_max_len,
        in_explain_stmt: false,
        warn_handler: None,
        extra_warnghandler: None,
    }
}

/// The Go generator's ppDump.
fn dump(
    ctx: &PlanCtx<'_>,
    bctx: &BuildPBContext,
    p: &dyn PhysicalPlan,
    root: bool,
) -> Result<Value, Error> {
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!(p.tp()));
    o.insert("id".into(), json!(p.id()));
    let any = p.as_any();
    let mut root_pb = false;
    match p.kind() {
        PhysicalKind::TableReader => {
            let x = any.downcast_ref::<PhysicalTableReader>().unwrap();
            let tbl = x.table_plan.as_ref().expect("tablePlan");
            o.insert("virt".into(), virt(x.schema()));
            o.insert("tbl".into(), dump(ctx, bctx, &**tbl, false)?);
            let pbs = x
                .table_plans
                .iter()
                .map(|tp| pb(tp.to_pb(bctx, TiKV)))
                .collect::<Result<Vec<_>, _>>()?;
            o.insert("pb".into(), Value::Array(pbs));
            o.insert("flash".into(), pb(tbl.to_pb(bctx, TiFlash))?);
            o.insert("acc".into(), acc(&*x.access_object(ctx)?));
        }
        PhysicalKind::TableScan => {
            let x = any.downcast_ref::<PhysicalTableScan>().unwrap();
            o.insert("cols".into(), cols(x.schema()));
            o.insert("acc".into(), acc(&*x.access_object()));
        }
        PhysicalKind::PointGet => {
            let x = any.downcast_ref::<PointGetPlan>().unwrap();
            o.insert("virt".into(), virt(&x.schema));
            o.insert("acc".into(), acc(&*x.access_object()));
        }
        PhysicalKind::Selection => {
            let x = any.downcast_ref::<PhysicalSelection>().unwrap();
            o.insert("conds".into(), idxs(&x.conditions));
            root_pb = true;
        }
        PhysicalKind::Projection => {
            let x = any.downcast_ref::<PhysicalProjection>().unwrap();
            o.insert("exprs".into(), idxs(&x.exprs));
            root_pb = true;
        }
        PhysicalKind::Sort => {
            let x = any.downcast_ref::<PhysicalSort>().unwrap();
            o.insert("by".into(), by(&x.by_items));
            root_pb = true;
        }
        PhysicalKind::NominalSort => {
            let x = any.downcast_ref::<NominalSort>().unwrap();
            o.insert("by".into(), by(&x.by_items));
        }
        PhysicalKind::TopN => {
            let x = any.downcast_ref::<PhysicalTopN>().unwrap();
            o.insert("by".into(), by(&x.by_items));
            o.insert("part".into(), part(&x.partition_by));
            o.insert("prefix".into(), prefix(&x.prefix_col));
            root_pb = true;
        }
        PhysicalKind::Limit => {
            let x = any.downcast_ref::<PhysicalLimit>().unwrap();
            o.insert("cols".into(), cols(x.schema()));
            o.insert("part".into(), part(&x.partition_by));
            o.insert("prefix".into(), prefix(&x.prefix_col));
            root_pb = true;
        }
        _ => {
            o.insert("other".into(), json!(true));
        }
    }
    if root && root_pb && !has_point_get(p) {
        o.insert("pb".into(), pb(p.to_pb(bctx, TiKV))?);
    }
    let children = p
        .children()
        .iter()
        .map(|c| dump(ctx, bctx, &**c, root))
        .collect::<Result<Vec<_>, _>>()?;
    o.insert("children".into(), Value::Array(children));
    Ok(Value::Object(o))
}

/// The plan of the `findbest` replay, then `f`.
fn with_plan(
    tables: &Arc<Tables>,
    args: &[Value],
    f: &mut dyn FnMut(&mut PlanCtx<'_>, PhysicalPlanRef) -> Result<Value, Error>,
) -> Outcome {
    with_built(
        tables,
        args[1].as_str().unwrap(),
        &args[2],
        &mut |ctx, flag, p| {
            let run = flag & !(FlagCollectPredicateColumnsPoint | FlagSyncWaitStatsLoadPoint);
            let (plan, _) = do_optimize_plan(ctx, run, p)?;
            f(ctx, plan)
        },
    )
}

fn run_plan(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    with_plan(tables, args, &mut |ctx, plan| {
        let bctx = build_pb_ctx(ctx);
        Ok(json!({ "plan": dump(ctx, &bctx, &*plan, true)? }))
    })
}

/// A step from a node to the next one of the Go generator's ppPreorder.
#[derive(Clone, Copy)]
enum Step {
    Table,
    Child(usize),
}

fn preorder(p: &dyn PhysicalPlan, path: &mut Vec<Step>, out: &mut Vec<Vec<Step>>) {
    out.push(path.clone());
    if let Some(r) = p.as_any().downcast_ref::<PhysicalTableReader>() {
        path.push(Step::Table);
        preorder(&**r.table_plan.as_ref().unwrap(), path, out);
        path.pop();
    }
    for (i, c) in p.children().iter().enumerate() {
        path.push(Step::Child(i));
        preorder(&**c, path, out);
        path.pop();
    }
}

fn node_at<'a>(p: &'a mut PhysicalPlanRef, path: &[Step]) -> &'a mut dyn PhysicalPlan {
    let node = make_mut_plan(p);
    match path.first() {
        None => node,
        Some(Step::Table) => {
            let r = node
                .as_any_mut()
                .downcast_mut::<PhysicalTableReader>()
                .unwrap();
            // The flattened list shares the table plan's nodes.
            r.table_plans.clear();
            node_at(r.table_plan.as_mut().unwrap(), &path[1..])
        }
        Some(Step::Child(i)) => node_at(&mut node.children_mut()[*i], &path[1..]),
    }
}

fn missing_col() -> Column {
    Column {
        unique_id: 1 << 20,
        ret_type: Arc::new(new_field_type(tidb_base::mysql::TypeLonglong)),
        ..Default::default()
    }
}

fn run_missing(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    let k = args[3].as_u64().unwrap() as usize;
    with_plan(tables, args, &mut |ctx, mut plan| {
        let mut paths = Vec::new();
        preorder(&*plan, &mut Vec::new(), &mut paths);
        if k >= paths.len() {
            return Ok(json!({"none": true}));
        }
        let p = node_at(&mut plan, &paths[k]);
        let tp = p.tp();
        let miss: ExprRef = Arc::new(Expr::Column(missing_col()));
        let kind = p.kind();
        let any = p.as_any_mut();
        match kind {
            PhysicalKind::Selection => any
                .downcast_mut::<PhysicalSelection>()
                .unwrap()
                .conditions
                .push(miss),
            PhysicalKind::Projection => any
                .downcast_mut::<PhysicalProjection>()
                .unwrap()
                .exprs
                .push(miss),
            PhysicalKind::Sort => {
                any.downcast_mut::<PhysicalSort>()
                    .unwrap()
                    .by_items
                    .push(ByItems {
                        expr: miss,
                        desc: false,
                    })
            }
            PhysicalKind::NominalSort => {
                any.downcast_mut::<NominalSort>()
                    .unwrap()
                    .by_items
                    .push(ByItems {
                        expr: miss,
                        desc: false,
                    })
            }
            PhysicalKind::TopN => {
                any.downcast_mut::<PhysicalTopN>()
                    .unwrap()
                    .by_items
                    .push(ByItems {
                        expr: miss,
                        desc: false,
                    })
            }
            PhysicalKind::Limit => {
                let x = any.downcast_mut::<PhysicalLimit>().unwrap();
                let mut cs = x.schema().columns.clone();
                cs.push(Arc::new(missing_col()));
                x.sp.set_schema(Some(new_schema(cs)));
            }
            _ => return Ok(json!({"tp": tp, "skip": true})),
        }
        match p.resolve_indices(ctx) {
            Err(e) if blocked_by(&e).is_none() => Ok(json!({"tp": tp, "err": gs(&e.error())})),
            Err(e) => Err(e),
            Ok(()) => Ok(json!({"tp": tp, "err": null})),
        }
    })
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

fn run_access(args: &[Value]) -> Value {
    match args[0].as_str().unwrap() {
        "dyn" => {
            // Go's nil list is JSON null.
            let objs = args[1]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|d| DynamicPartitionAccessObject {
                            database: d["db"].as_str().unwrap().to_owned(),
                            table: d["table"].as_str().unwrap().to_owned(),
                            all_partitions: d["all"].as_bool().unwrap(),
                            partitions: strs(&d["parts"]),
                            err: d["err"].as_str().unwrap().to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            acc(&DynamicPartitionAccessObjects(objs))
        }
        "scan" => {
            let s = &args[1];
            let indexes = s["idx"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|i| IndexAccess {
                            name: i["name"].as_str().unwrap().to_owned(),
                            cols: strs(&i["cols"]),
                            is_clustered_index: i["clustered"].as_bool().unwrap(),
                        })
                        .collect()
                })
                .unwrap_or_default();
            acc(&ScanAccessObject {
                database: s["db"].as_str().unwrap().to_owned(),
                table: s["table"].as_str().unwrap().to_owned(),
                indexes,
                partitions: strs(&s["parts"]),
            })
        }
        "other" => acc(&OtherAccessObject(args[1].as_str().unwrap().to_owned())),
        k => panic!("access kind {k}"),
    }
}

#[test]
fn govec_postprocess() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "postprocess");
    let mut tables: Option<Arc<Tables>> = None;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let (mut diffs, mut unparsed) = (Vec::new(), Vec::new());
    let mut records = 0usize;
    let list = std::env::var("GOVEC_POSTPROCESS_BLOCKED").is_ok();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        records += 1;
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        assert!(rec["err"].is_null(), "Go error in {op} {args:?}");
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
        if op == "access" {
            let got = run_access(args);
            if &got == out {
                *counts.entry(op.to_owned()).or_default() += 1;
            } else {
                diffs.push(format!(
                    "access {args:?}: {}",
                    first_diff(out, &got, String::new()).unwrap_or_default()
                ));
            }
            continue;
        }
        let tables = tables.as_ref().expect("tables first");
        let sql = args[1].as_str().unwrap();
        let outcome = match op {
            "plan" => run_plan(tables, args),
            "missing" => run_missing(tables, args),
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
                    eprintln!("blocked {b} ({op}): {sql}");
                }
                *blocked.entry(format!("{op} {b}")).or_default() += 1;
                continue;
            }
            Outcome::Value(v) => v,
        };
        if &got == out {
            *counts.entry(op.to_owned()).or_default() += 1;
        } else {
            diffs.push(format!(
                "{op} {sql} {}: {}",
                args.get(3).map(|k| k.to_string()).unwrap_or_default(),
                first_diff(out, &got, String::new()).unwrap_or_default()
            ));
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(records as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "postprocess: {records} records, equal to Go {counts:?}, blocked {blocked:?}, {} not parsed, {} differences",
        unparsed.len(),
        diffs.len()
    );
    for d in diffs.iter().take(30) {
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
