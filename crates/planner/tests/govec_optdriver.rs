//! Replays the govec `optdriver` vectors (the optimizer driver of the first vertical slice,
//! specs/D09.13-optimizer) against Go TiDB v8.5.8 (tracker `govec/domains/optdriver.go`,
//! which documents the record format):
//!
//! - `optimize`: a query is preprocessed, walked by the query block hint handler, built with
//!   it, its flags adjusted (`adjustOptimizationFlags`) and logically optimized
//!   (`logicalOptimize`, without the two statistics loading points, as Go's generator); the
//!   optimized logical plan must equal Go's;
//! - `post`: a hand-built physical plan of the slice's operators goes through `postOptimize`
//!   (projection elimination, `InjectExtraProjection`, the selection merge, ...); the result
//!   must equal Go's;
//! - `fix`, `max_one_row`, `index_key`: the fix control lookups and the key-info helpers.
//!
//! The physical optimization (`physicalOptimize`) is not replayed: it needs FindBestTask
//! (the next task of the slice); the final plans come with its EXPLAIN vectors.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::contextutil::{StaticWarnHandler, WarnHandler, new_static_warn_handler};
use tidb_base::errors::RedactLogDisable;
use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{NotNullFlag, SQLMode, TypeLonglong};
use tidb_expression::scalar_function::new_function;
use tidb_expression::{Column, Constant, Expr, ExprRef, KeyInfo, Schema, new_schema};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::optimizer::{
    adjust_optimization_flags, logical_optimize_test, post_optimize,
};
use tidb_planner::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableDual, PhysicalTopN,
};
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::planbuilder::new_plan_builder;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::core::rule::logical_rules::{
    FlagCollectPredicateColumnsPoint, FlagSyncWaitStatsLoadPoint,
};
use tidb_planner::core::rule::util::build_key_info_misc::{
    check_index_can_be_key, check_max_one_row_cond,
};
use tidb_planner::hint::hint_query_block::new_qb_hint_handler;
use tidb_planner::logicalop::{
    DataSource, FrameBound, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort,
    LogicalTableDual, LogicalTopN, LogicalWindow,
};
use tidb_planner::property::{PhysicalProperty, StatsInfo};
use tidb_planner::util::fixcontrol;
use tidb_planner::util::{AccessPath, ByItems, HandleCols};
use tidb_planner::{LogicalKind, PhysicalKind, PhysicalPlan, PhysicalPlanRef, PlanCtx, PlanId};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::collate;
use tidb_types::datum::new_int_datum;
use tidb_types::field_name::NameSlice;
use tidb_types::field_type::new_field_type;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// The `optimize` records whose Rust result is a boundary stub, per block, in the committed
/// vectors (the same on both targets; constitution §7.3). A boundary that starts answering,
/// or a new one, fails here: update the pin in the same PR. Set `GOVEC_OPTDRIVER_BLOCKED=1`
/// to list the statements.
const EXPECTED_BLOCKED: &[(&str, usize)] = &[
    // Comment-style hints (`/*+ USE_INDEX(...) */`): the parser keeps them opaque.
    ("D02.2", 2),
    // Time functions not ported yet (DATE_ADD / DATE_SUB for window RANGE INTERVAL bounds).
    ("D08.7", 2),
    // Built-in function families not ported yet (IN, ANY_VALUE, user variables, ...).
    ("D08.9", 61),
    // Virtual generated columns.
    ("D09.1", 2),
    // Partitioned tables (2 of them: Go's mock context panics in the partition pruning).
    ("D09.12", 4),
    // BuildKeySolver in logicalOptimize (reached by aggregation / GROUP BY / DISTINCT queries).
    ("D09.13", 103),
    // The lock operator (SELECT ... FOR UPDATE).
    ("D09.14", 2),
    // System variables in expressions.
    ("D09.2", 2),
    // Joins.
    ("D09.3", 6),
    // GROUP BY ... WITH ROLLUP (D09.4 part C).
    ("D09.4", 2),
    // WITH and set operations.
    ("D09.5", 4),
    // Subqueries.
    ("D09.6", 2),
    // Index hints (USE / FORCE / IGNORE INDEX).
    ("D13.3", 2),
];

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/optdriver.jsonl.gz",
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
        LogicalKind::TopN => {
            let x = node.as_any().downcast_ref::<LogicalTopN>().unwrap();
            o.insert("by".into(), by_out(ectx, &x.by_items));
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        LogicalKind::Window => {
            let x = node.as_any().downcast_ref::<LogicalWindow>().unwrap();
            let wfs: Vec<Value> = x
                .window_func_descs
                .iter()
                .map(|w| {
                    json!([
                        w.base.name,
                        exprs_out(ectx, &w.base.args),
                        ft_out(&w.base.ret_tp),
                    ])
                })
                .collect();
            let pby: Vec<Value> = x
                .partition_by
                .iter()
                .map(|it| {
                    json!([
                        expr_out(ectx, &Expr::Column(Column::clone(&it.col))),
                        it.desc
                    ])
                })
                .collect();
            let oby: Vec<Value> = x
                .order_by
                .iter()
                .map(|it| {
                    json!([
                        expr_out(ectx, &Expr::Column(Column::clone(&it.col))),
                        it.desc
                    ])
                })
                .collect();
            let frame = match &x.frame {
                None => Value::Null,
                Some(fr) => {
                    let dump_bound = |b: &Option<FrameBound>| -> Value {
                        match b {
                            None => Value::Null,
                            Some(b) => json!([
                                b.tp as i32,
                                b.unbounded,
                                b.num,
                                exprs_out(ectx, &b.calc_funcs),
                                b.cmp_data_type as i32,
                                b.is_explicit_range,
                            ]),
                        }
                    };
                    json!({
                        "type": fr.tp as i32,
                        "start": dump_bound(&fr.start),
                        "end": dump_bound(&fr.end),
                    })
                }
            };
            o.insert("wfs".into(), Value::Array(wfs));
            o.insert("pby".into(), Value::Array(pby));
            o.insert("oby".into(), Value::Array(oby));
            o.insert("frame".into(), frame);
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

// ---------------------------------------------------------------- optimize

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

fn run_optimize(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
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
        let run = flag & !(FlagCollectPredicateColumnsPoint | FlagSyncWaitStatsLoadPoint);
        let p = logical_optimize_test(&mut ctx, run, p)?;
        Ok::<_, Error>((flag, p))
    }));
    match res {
        Err(_) => Outcome::Value(json!({"panic": true})),
        Ok(Err(e)) => blocked_or(e),
        Ok(Ok((flag, p))) => {
            let plan = dump(&ctx, p);
            let plan_id = ctx.vars.state.plan_id.0.load(Ordering::SeqCst);
            let col_id = ctx.vars.state.plan_column_id.0.load(Ordering::SeqCst);
            let w = warns(ctx.vars, &wh);
            Outcome::Value(json!({
                "flag": flag,
                "plan": plan,
                "plan_id": plan_id,
                "col_id": col_id,
                "w": w,
            }))
        }
    }
}

// ---------------------------------------------------------------- post

fn stats(rows: f64) -> Option<Arc<StatsInfo>> {
    Some(Arc::new(StatsInfo {
        row_count: rows,
        ..Default::default()
    }))
}

fn longlong() -> FieldType {
    new_field_type(TypeLonglong)
}

/// An expr spec of the record (see the Go generator's odExpr).
fn build_expr(ctx: &PlanCtx<'_>, v: &Value, schema: &Schema) -> ExprRef {
    let a = v.as_array().unwrap();
    let int_const = |k: i64| Arc::new(Expr::Constant(Constant::new(new_int_datum(k), longlong())));
    match a[0].as_str().unwrap() {
        "col" => {
            let c = &schema.columns[a[1].as_u64().unwrap() as usize];
            Arc::new(Expr::Column(Column::clone(c)))
        }
        "const" => int_const(a[1].as_i64().unwrap()),
        "plus" => {
            let x = build_expr(ctx, &a[1], schema);
            let c = int_const(a[2].as_i64().unwrap());
            new_function(&*ctx.expr_ctx, "plus", longlong(), vec![x, c]).unwrap()
        }
        k => panic!("expr {k}"),
    }
}

/// The physical plan of a spec (the Go generator's odBuild, in the same order: children
/// first, then the node's Init, then its new columns).
fn build_plan(ctx: &PlanCtx<'_>, s: &Value) -> PhysicalPlanRef {
    let children: Vec<PhysicalPlanRef> = s["children"]
        .as_array()
        .map(|cs| cs.iter().map(|c| build_plan(ctx, c)).collect())
        .unwrap_or_default();
    let child_schema = children
        .first()
        .map(|c| c.schema().clone())
        .unwrap_or_default();
    let rows = s["rows"].as_f64().unwrap_or(0.0);
    let prop = || {
        vec![Some(Arc::new(PhysicalProperty {
            expected_cnt: s["exp"].as_f64().unwrap_or(0.0),
            ..Default::default()
        }))]
    };
    let exprs = |key: &str| -> Vec<ExprRef> {
        s[key]
            .as_array()
            .map(|es| {
                es.iter()
                    .map(|e| build_expr(ctx, e, &child_schema))
                    .collect()
            })
            .unwrap_or_default()
    };
    let by_items = || -> Vec<ByItems> {
        s["by"]
            .as_array()
            .map(|bs| {
                bs.iter()
                    .map(|b| ByItems {
                        expr: build_expr(ctx, &b[0], &child_schema),
                        desc: b[1].as_bool().unwrap(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut p: PhysicalPlanRef = match s["op"].as_str().unwrap() {
        "dual" => {
            let mut d = PhysicalTableDual {
                row_count: 1,
                ..Default::default()
            }
            .init(ctx, stats(rows), 0);
            let n = s["cols"].as_u64().unwrap() as usize;
            let cols = (0..n)
                .map(|i| {
                    Arc::new(Column {
                        unique_id: ctx.vars.alloc_plan_column_id(),
                        ret_type: Arc::new(longlong()),
                        index: i as isize,
                        ..Default::default()
                    })
                })
                .collect();
            d.set_schema(new_schema(cols));
            Arc::new(d)
        }
        "proj" => {
            let es = exprs("exprs");
            let mut proj = PhysicalProjection {
                exprs: es.clone(),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop());
            let ectx = ctx.eval_ctx();
            let cols = es
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    if let Expr::Column(c) = &**e {
                        let mut cc = c.clone();
                        cc.index = i as isize;
                        return Arc::new(cc);
                    }
                    Arc::new(Column {
                        unique_id: ctx.vars.alloc_plan_column_id(),
                        ret_type: Arc::new(e.get_type(&*ectx).into_owned()),
                        index: i as isize,
                        ..Default::default()
                    })
                })
                .collect();
            proj.set_schema(new_schema(cols));
            Arc::new(proj)
        }
        "sel" => Arc::new(
            PhysicalSelection {
                conditions: exprs("exprs"),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop()),
        ),
        "sort" => Arc::new(
            PhysicalSort {
                by_items: by_items(),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop()),
        ),
        "topn" => Arc::new(
            PhysicalTopN {
                by_items: by_items(),
                offset: s["offset"].as_u64().unwrap_or(0),
                count: s["count"].as_u64().unwrap_or(0),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop()),
        ),
        "nsort" => Arc::new(
            NominalSort {
                by_items: by_items(),
                only_column: s["only_col"].as_bool().unwrap_or(false),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop()),
        ),
        "limit" => {
            let mut l = PhysicalLimit {
                offset: s["offset"].as_u64().unwrap_or(0),
                count: s["count"].as_u64().unwrap_or(0),
                ..Default::default()
            }
            .init(ctx, stats(rows), 0, prop());
            l.set_schema(child_schema.clone());
            Arc::new(l)
        }
        op => panic!("op {op}"),
    };
    if !children.is_empty() {
        Arc::get_mut(&mut p).unwrap().set_children(children);
    }
    p
}

fn phys_dump(ctx: &PlanCtx<'_>, p: &PhysicalPlanRef) -> Value {
    let ectx = ctx.eval_ctx();
    let ectx = &*ectx;
    let rows = p.stats_info().map_or(0.0, |s| s.row_count);
    let mut o = serde_json::Map::new();
    o.insert("tp".into(), json!(p.tp()));
    o.insert("id".into(), json!(p.id()));
    o.insert("rows".into(), json!(format_float(rows, b'g', -1, 64)));
    o.insert("schema".into(), cols_out(ectx, &p.schema().columns));
    match p.kind() {
        PhysicalKind::Projection => {
            let x = p.as_any().downcast_ref::<PhysicalProjection>().unwrap();
            o.insert("exprs".into(), exprs_out(ectx, &x.exprs));
            o.insert("avoid".into(), json!(x.avoid_column_evaluator));
        }
        PhysicalKind::Selection => {
            let x = p.as_any().downcast_ref::<PhysicalSelection>().unwrap();
            o.insert("conds".into(), exprs_out(ectx, &x.conditions));
        }
        PhysicalKind::Sort => {
            let x = p.as_any().downcast_ref::<PhysicalSort>().unwrap();
            o.insert("by".into(), by_out(ectx, &x.by_items));
        }
        PhysicalKind::TopN => {
            let x = p.as_any().downcast_ref::<PhysicalTopN>().unwrap();
            o.insert("by".into(), by_out(ectx, &x.by_items));
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        PhysicalKind::Limit => {
            let x = p.as_any().downcast_ref::<PhysicalLimit>().unwrap();
            o.insert("offset".into(), json!(x.offset));
            o.insert("count".into(), json!(x.count));
        }
        PhysicalKind::NominalSort => {
            let x = p.as_any().downcast_ref::<NominalSort>().unwrap();
            o.insert("by".into(), by_out(ectx, &x.by_items));
        }
        PhysicalKind::TableDual => {
            let x = p.as_any().downcast_ref::<PhysicalTableDual>().unwrap();
            o.insert("dual_rows".into(), json!(x.row_count));
        }
        k => panic!("phys_dump {k:?}"),
    }
    let children: Vec<Value> = p.children().iter().map(|c| phys_dump(ctx, c)).collect();
    o.insert("children".into(), Value::Array(children));
    Value::Object(o)
}

fn run_post(args: &[Value]) -> Result<Value, Error> {
    collate::set_new_collation_enabled_for_test(true);
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let wh = Arc::new(new_static_warn_handler(0));
    let mut ctx = PlanCtx::new(&mut vars, new_expr_ctx(0, &wh));
    let plan = build_plan(&ctx, &args[0]);
    let plan = post_optimize(&mut ctx, plan)?;
    Ok(json!({
        "plan": phys_dump(&ctx, &plan),
        "col_id": ctx.vars.state.plan_column_id.0.load(Ordering::SeqCst),
        "plan_id": ctx.vars.state.plan_id.0.load(Ordering::SeqCst),
    }))
}

// ---------------------------------------------------------------- helpers of the block

fn go_float(f: f64) -> Value {
    if f == f64::INFINITY {
        return json!("+Inf");
    }
    json!(format_float(f, b'g', -1, 64))
}

fn run_fix(args: &[Value]) -> Value {
    let m: HashMap<u64, String> = args[0]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.parse().unwrap(), v.as_str().unwrap().to_owned()))
        .collect();
    let key = args[1].as_u64().unwrap();
    match args[2].as_str().unwrap() {
        "bool" => {
            let (v, ok) = fixcontrol::get_bool(&m, key);
            json!([
                v,
                ok,
                false,
                fixcontrol::get_bool_with_default(&m, key, args[3].as_bool().unwrap())
            ])
        }
        "int" => {
            let (v, ok, e) = fixcontrol::get_int(&m, key);
            json!([
                v,
                ok,
                e,
                fixcontrol::get_int_with_default(&m, key, args[3].as_i64().unwrap())
            ])
        }
        "float" => {
            let (v, ok, e) = fixcontrol::get_float(&m, key);
            json!([
                go_float(v),
                ok,
                e,
                go_float(fixcontrol::get_float_with_default(
                    &m,
                    key,
                    args[3].as_f64().unwrap()
                ))
            ])
        }
        "str" => {
            let (v, ok) = fixcontrol::get_str(&m, key);
            json!([
                v,
                ok,
                false,
                fixcontrol::get_str_with_default(&m, key, args[3].as_str().unwrap())
            ])
        }
        k => panic!("fix kind {k}"),
    }
}

fn key_of(v: &Value) -> KeyInfo {
    v.as_array()
        .unwrap()
        .iter()
        .map(|id| {
            Arc::new(Column {
                unique_id: id.as_i64().unwrap(),
                ..Default::default()
            })
        })
        .collect()
}

fn key_out(k: Option<KeyInfo>) -> Value {
    match k {
        None => Value::Null,
        Some(k) => Value::Array(k.iter().map(|c| json!(c.unique_id)).collect()),
    }
}

fn run_max_one_row(args: &[Value]) -> Value {
    let mut s = new_schema(Vec::new());
    s.keys = args[0].as_array().unwrap().iter().map(key_of).collect();
    s.unique_keys = args[1].as_array().unwrap().iter().map(key_of).collect();
    let eq: HashSet<i64> = args[2]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    json!(check_max_one_row_cond(&eq, &s))
}

fn run_index_key(args: &[Value]) -> Value {
    let idx = IndexInfo {
        unique: args[0].as_bool().unwrap(),
        columns: Some(
            args[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| IndexColumn {
                    name: CIStr::new(n.as_str().unwrap()),
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    };
    let mut infos = Vec::new();
    let mut cols = Vec::new();
    for (i, c) in args[2].as_array().unwrap().iter().enumerate() {
        let mut ft = longlong();
        if c[1].as_bool().unwrap() {
            ft.add_flag(NotNullFlag);
        }
        infos.push(Arc::new(ColumnInfo {
            name: CIStr::new(c[0].as_str().unwrap()),
            field_type: ft.clone(),
            ..Default::default()
        }));
        cols.push(Arc::new(Column {
            unique_id: 100 + i as i64,
            ret_type: Arc::new(ft),
            ..Default::default()
        }));
    }
    let (uk, nk) = check_index_can_be_key(&idx, &infos, &new_schema(cols));
    json!([key_out(uk), key_out(nk)])
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

/// Records in `optdriver` whose `plan.names` differs from Go v8.5.8 strictly because of ruling
/// `R32` (`specs/D04.1-privilege/spec.md` §5.2 item 1, §6.1 D5 option A): `tidb-rs` populates
/// `orig_tbl_name = table_info.name` on implicit `_tidb_rowid` columns where Go left `""`.
const EXPECTED_R32_NORMALIZED: usize = 901;

fn normalize_r32_rowid_orig_tbl_in_plan(plan: &mut Value) -> usize {
    let mut count = 0;
    if let Some(obj) = plan.as_object_mut() {
        if let Some(names) = obj.get_mut("names").and_then(Value::as_array_mut) {
            for entry in names {
                if let Some(arr) = entry.as_array_mut()
                    && arr.len() >= 5
                    && arr[2].as_str() == Some("_tidb_rowid")
                    && arr[4].as_str() == Some("_tidb_rowid")
                    && arr[3].as_str().is_some_and(|s| !s.is_empty())
                {
                    arr[3] = json!("");
                    count += 1;
                }
            }
        }
        if let Some(children) = obj.get_mut("children").and_then(Value::as_array_mut) {
            for c in children {
                count += normalize_r32_rowid_orig_tbl_in_plan(c);
            }
        }
    }
    count
}

fn matches_with_r32_rowid_normalization(out: &Value, got: &Value) -> bool {
    let mut norm = got.clone();
    let changed = norm
        .get_mut("plan")
        .map_or(0, normalize_r32_rowid_orig_tbl_in_plan);
    changed > 0 && &norm == out
}

#[test]
fn govec_optdriver() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "optdriver");
    let mut tables: Option<Arc<Tables>> = None;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let (mut diffs, mut unparsed) = (Vec::new(), Vec::new());
    let mut records = 0usize;
    let mut r32_normalized = 0usize;
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        records += 1;
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        if !rec["err"].is_null() {
            // Go's mock context panics in the partition pruning of a partitioned table (no
            // partition expression cache); tidb-rs stops at the partitioned table before.
            match (
                op,
                run_optimize(tables.as_ref().expect("tables first"), args),
            ) {
                ("optimize", Outcome::Blocked(b)) if rec["err"]["name"] == "panic" => {
                    *blocked.entry(b).or_default() += 1
                }
                _ => diffs.push(format!(
                    "{op} {}: Go returned an error {}",
                    args[1], rec["err"]
                )),
            }
            continue;
        }
        let got = match op {
            "tables" => {
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
            "optimize" => {
                let sql = args[1].as_str().unwrap();
                match run_optimize(tables.as_ref().expect("tables first"), args) {
                    Outcome::Unparsed => {
                        if out["code"] != 1064 {
                            unparsed.push(sql.to_owned());
                        }
                        continue;
                    }
                    Outcome::Blocked(b) => {
                        if std::env::var("GOVEC_OPTDRIVER_BLOCKED").is_ok() {
                            eprintln!("blocked {b}: {sql}");
                        }
                        *blocked.entry(b).or_default() += 1;
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
                }
            }
            "post" => match run_post(args) {
                Ok(v) => v,
                Err(e) => json!({"rust_err": e.to_string()}),
            },
            "fix" => run_fix(args),
            "max_one_row" => run_max_one_row(args),
            "index_key" => run_index_key(args),
            op => panic!("op {op}"),
        };
        if &got == out {
            *counts.entry(op.to_owned()).or_default() += 1;
        } else if op == "optimize" && matches_with_r32_rowid_normalization(out, &got) {
            *counts.entry(op.to_owned()).or_default() += 1;
            r32_normalized += 1;
        } else {
            diffs.push(format!(
                "{op} {}: {}",
                args.first()
                    .map(|a| a.to_string())
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect::<String>(),
                first_diff(out, &got, String::new()).unwrap_or_default()
            ));
        }
    }
    collate::set_new_collation_enabled_for_test(true);
    assert_eq!(records as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "optdriver: {records} records, equal to Go {counts:?} ({r32_normalized} R32-normalized), blocked {blocked:?}, {} not parsed, {} differences",
        unparsed.len(),
        diffs.len()
    );
    for d in diffs.iter().take(20) {
        eprintln!("{d}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
    assert_eq!(
        r32_normalized, EXPECTED_R32_NORMALIZED,
        "R32-normalized record count changed"
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
