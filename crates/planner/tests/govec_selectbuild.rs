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
use tidb_base::gostd::strconv::format_float;
use tidb_base::gostd::time::utc;
use tidb_base::kv::{TiFlash, TiKV};
use tidb_base::mysql::SQLMode;
use tidb_expression::aggregation::AggFunctionMode;
use tidb_expression::{Column, Expr};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::TableInfo;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::arena::with_detached;
use tidb_planner::base as pbase;
use tidb_planner::core::plan_cache_utils::InfoSchemaLookup;
use tidb_planner::core::planbuilder::new_plan_builder;
use tidb_planner::core::preprocess::{
    InfoSchemaHandle, PreprocessorReturn, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_planner::hint::hint::{PreferHashAgg, PreferStreamAgg};
use tidb_planner::logicalop::logical_join::build_logical_join_schema;
use tidb_planner::logicalop::{
    DataSource, FrameBound, JoinType, LogicalAggregation, LogicalJoin, LogicalLimit,
    LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual, LogicalWindow,
};
use tidb_planner::property::{GroupNDV, StatsInfo};
use tidb_planner::util::optimizetrace::default_logical_optimize_option;
use tidb_planner::util::{AccessPath, ByItems, HandleCols};
use tidb_planner::{LogicalKind, LogicalPlan, PlanCtx, PlanId};
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
    // Time functions not ported yet (DATE_ADD / DATE_SUB for window RANGE INTERVAL bounds).
    ("D08.7", 2),
    // Built-in function families not ported yet (IN, ANY_VALUE, user variables, ...).
    ("D08.9", 65),
    // Virtual generated columns.
    ("D09.1", 2),
    // Partitioned tables (2 of them in a join).
    ("D09.12", 6),
    // The lock operator (SELECT ... FOR UPDATE).
    ("D09.14", 2),
    // System variables in expressions (the rewriter's part 2).
    ("D09.2", 2),
    // GROUP BY ... WITH ROLLUP (D09.4 part C).
    ("D09.4", 2),
    // WITH and set operations.
    ("D09.5", 4),
    // Subqueries (2 of them in the ON condition of a join).
    ("D09.6", 4),
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
        LogicalKind::Aggregation => {
            let x = node.as_any().downcast_ref::<LogicalAggregation>().unwrap();
            let aggs: Vec<Value> = x
                .agg_funcs
                .iter()
                .map(|a| {
                    json!([
                        a.base.name,
                        exprs_out(ectx, &a.base.args),
                        a.has_distinct,
                        a.mode as i32,
                        by_out(ectx, &a.order_by_items),
                        ft_out(&a.base.ret_tp),
                    ])
                })
                .collect();
            o.insert("aggs".into(), Value::Array(aggs));
            o.insert("gby".into(), exprs_out(ectx, &x.group_by_items));
            o.insert("prefer_agg_type".into(), json!(x.prefer_agg_type));
            o.insert("prefer_agg_to_cop".into(), json!(x.prefer_agg_to_cop));
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
        LogicalKind::Join => {
            let x = node.as_any().downcast_ref::<LogicalJoin>().unwrap();
            o.insert("join_type".into(), json!(x.join_type as i32));
            o.insert("reordered".into(), json!(x.reordered));
            o.insert("cartesian".into(), json!(x.cartesian_join));
            o.insert("straight".into(), json!(x.straight_join));
            o.insert("prefer_join_type".into(), json!(x.prefer_join_type));
            o.insert("prefer_join_order".into(), json!(x.prefer_join_order));
            o.insert(
                "left_prefer_join_type".into(),
                json!(x.left_prefer_join_type),
            );
            o.insert(
                "right_prefer_join_type".into(),
                json!(x.right_prefer_join_type),
            );
            o.insert("eq".into(), exprs_out(ectx, &x.equal_conditions));
            o.insert("na_eq".into(), exprs_out(ectx, &x.na_eq_conditions));
            o.insert("left".into(), exprs_out(ectx, &x.left_conditions));
            o.insert("right".into(), exprs_out(ectx, &x.right_conditions));
            o.insert("other".into(), exprs_out(ectx, &x.other_conditions));
            o.insert(
                "full_schema".into(),
                x.full_schema
                    .as_ref()
                    .map_or(Value::Null, |fs| cols_out(ectx, &fs.columns)),
            );
            o.insert("full_names".into(), names_out(&x.full_names));
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
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(err_out(&e));
        }
        node.get_resolve_context()
    };

    // BuildLogicalPlanForTest: plan and column ids from 0.
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

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
            let mut resolved_tables: Vec<(String, String)> = builder
                .resolve_ctx
                .as_ref()
                .map(|rc| {
                    rc.lock()
                        .get_table_names()
                        .values()
                        .map(|tn| {
                            let db = tn
                                .db_info
                                .as_ref()
                                .filter(|d| !d.name.l.is_empty())
                                .map_or_else(|| "test".to_owned(), |d| d.name.l.clone());
                            (db, tn.table_info.name.l.clone())
                        })
                        .collect()
                })
                .unwrap_or_default();
            resolved_tables.sort();
            let has_group_by = stmt_has_group_by(&stmt);
            Outcome::Value(json!({
                "plan": plan,
                "plan_id": plan_id,
                "col_id": col_id,
                "opt_flag": builder.get_opt_flag(),
                "vi": vi,
                "w": w,
                "__r29_tables": resolved_tables,
                "__r29_has_gby": has_group_by,
            }))
        }
    }
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

fn find_agg(ctx: &PlanCtx<'_>, p: PlanId) -> Option<PlanId> {
    let node = ctx.arena.get(p);
    if node.kind() == LogicalKind::Aggregation {
        return Some(p);
    }
    node.children().iter().find_map(|&c| find_agg(ctx, c))
}

fn run_aggop(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    collate::set_new_collation_enabled_for_test(true);
    let sql = args[1].as_str().unwrap();
    let spec = &args[2];
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.sql_mode = mode;
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);
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
            return Outcome::Value(err_out(&e));
        }
        node.get_resolve_context()
    };
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

    let wh = Arc::new(new_static_warn_handler(0));
    let ev = es::new_eval_context(vec![
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
    ctx.client = Some(Arc::new(MockClient));
    let lp = match builder.build(&mut ctx, &stmt) {
        Ok(p) => p,
        Err(e) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(err_out(&e));
        }
    };
    let agg_id = find_agg(&ctx, lp).unwrap_or_else(|| {
        panic!(
            "no LogicalAggregation in {sql} ({spec}): {}",
            dump(&ctx, lp)
        )
    });
    let kind = spec["kind"].as_str().unwrap();
    match kind {
        "inspect" => {
            let agg = ctx
                .arena
                .downcast_ref::<LogicalAggregation>(agg_id)
                .unwrap();
            let gby_cols: Vec<Value> = agg
                .get_group_by_cols()
                .iter()
                .map(|c| json!(c.unique_id))
                .collect();
            let part_keys: Vec<Value> = agg
                .get_potential_partition_keys()
                .iter()
                .map(|k| json!([k.col.unique_id, k.collate_id]))
                .collect();
            let used_cols: Vec<Value> = agg
                .get_used_cols()
                .iter()
                .map(|c| json!(c.unique_id))
                .collect();
            let col_groups: Vec<Value> = agg
                .extract_col_groups(&ctx.arena, &[])
                .iter()
                .map(|g| Value::Array(g.iter().map(|c| json!(c.unique_id)).collect()))
                .collect();
            let cor_cols: Vec<Value> = agg
                .extract_correlated_cols()
                .iter()
                .map(|c| json!(c.column.unique_id))
                .collect();
            let cop_tikv = agg.can_push_to_cop(&ctx, TiKV);
            let cop_tiflash = agg.can_push_to_cop(&ctx, TiFlash);
            let can_pull_up = agg.can_pull_up(&ctx).unwrap();
            let distinct_meets = agg.distinct_args_meets_property(&ctx);
            let has_distinct = agg.has_distinct();
            let has_order_by = agg.has_order_by();
            let is_partial = !agg.agg_funcs.is_empty() && agg.is_partial_mode_agg();
            ctx.arena
                .downcast_mut::<LogicalAggregation>(agg_id)
                .unwrap()
                .no_cop_push_down = true;
            let cop_blocked = ctx
                .arena
                .downcast_ref::<LogicalAggregation>(agg_id)
                .unwrap()
                .can_push_to_cop(&ctx, TiKV);
            Outcome::Value(json!({
                "has_distinct": has_distinct,
                "has_order_by": has_order_by,
                "is_partial": is_partial,
                "gby_cols": gby_cols,
                "part_keys": part_keys,
                "used_cols": used_cols,
                "cop_tikv": cop_tikv,
                "cop_tiflash": cop_tiflash,
                "cop_blocked": cop_blocked,
                "can_pull_up": can_pull_up,
                "distinct_meets": distinct_meets,
                "col_groups": col_groups,
                "cor_cols": cor_cols,
            }))
        }
        "hints" => {
            let prefer = match spec["prefer"].as_str().unwrap_or("") {
                "hash" => PreferHashAgg,
                "stream" => PreferStreamAgg,
                "both" => PreferHashAgg | PreferStreamAgg,
                _ => 0,
            };
            ctx.arena
                .downcast_mut::<LogicalAggregation>(agg_id)
                .unwrap()
                .prefer_agg_type = prefer;
            let (ph, ps, final_pref) = with_detached(&mut ctx, agg_id, |node, ctx| {
                let agg = node
                    .as_any_mut()
                    .downcast_mut::<LogicalAggregation>()
                    .unwrap();
                let (ph, ps) = agg.reset_hint_if_conflicted(ctx);
                (ph, ps, agg.prefer_agg_type)
            });
            let w = warns(ctx.vars, &wh);
            Outcome::Value(json!({
                "hash": ph,
                "stream": ps,
                "prefer": final_pref,
                "w": w,
            }))
        }
        "keyinfo" => {
            let partial = spec["partial"].as_bool().unwrap_or(false);
            let self_only = spec["self_only"].as_bool().unwrap_or(false);
            let (keys, max_one_row) = with_detached(&mut ctx, agg_id, |node, ctx| {
                let agg = node
                    .as_any_mut()
                    .downcast_mut::<LogicalAggregation>()
                    .unwrap();
                if partial && !agg.agg_funcs.is_empty() {
                    agg.agg_funcs[0].mode = AggFunctionMode::Partial1Mode;
                }
                if self_only {
                    let mut s = agg.schema(&ctx.arena).clone();
                    agg.build_self_key_info(&mut s);
                    agg.sp.set_schema(Some(s));
                } else {
                    agg.build_key_info(&mut ctx.arena);
                }
                let keys: Vec<Value> = agg
                    .schema(&ctx.arena)
                    .keys
                    .iter()
                    .map(|k| Value::Array(k.iter().map(|c| json!(c.unique_id)).collect()))
                    .collect();
                (keys, agg.max_one_row())
            });
            Outcome::Value(json!({
                "keys": keys,
                "max_one_row": max_one_row,
            }))
        }
        "replace" => {
            let child_id = ctx.arena.get(agg_id).children()[0];
            let old_col = Arc::clone(&ctx.arena.get(child_id).schema(&ctx.arena).columns[0]);
            let mut new_col = (*old_col).clone();
            new_col.unique_id = 9999;
            let mut replace = std::collections::HashMap::new();
            replace.insert(old_col.hash_code().to_vec(), Arc::new(new_col));
            ctx.arena.get_mut(agg_id).replace_expr_columns(&replace);
            Outcome::Value(json!({"plan": dump(&ctx, agg_id)}))
        }
        "ppd" => {
            let opt = default_logical_optimize_option();
            let mut preds = Vec::new();
            if spec["const_pred"].as_bool().unwrap_or(false) {
                preds.push(Arc::new(Expr::Constant(tidb_expression::new_zero())));
            }
            match pbase::predicate_push_down(&mut ctx, lp, preds, &opt) {
                Ok((rem, new_p)) => {
                    let ectx = ctx.eval_ctx();
                    let rem_val = exprs_out(&*ectx, &rem);
                    drop(ectx);
                    Outcome::Value(json!({"plan": dump(&ctx, new_p), "rem": rem_val}))
                }
                Err(e) => Outcome::Value(err_out(&e)),
            }
        }
        "prune" => {
            let opt = default_logical_optimize_option();
            let used: Vec<Arc<Column>> = spec["keep"]
                .as_array()
                .map(|arr| {
                    let cols = &ctx.arena.get(lp).schema(&ctx.arena).columns;
                    arr.iter()
                        .map(|i| Arc::clone(&cols[i.as_u64().unwrap() as usize]))
                        .collect()
                })
                .unwrap_or_default();
            match pbase::prune_columns(&mut ctx, lp, used, &opt) {
                Ok(new_p) => Outcome::Value(json!({"plan": dump(&ctx, new_p)})),
                Err(e) => Outcome::Value(err_out(&e)),
            }
        }
        "stats" => {
            let child_id = ctx.arena.get(agg_id).children()[0];
            let child_cols = ctx.arena.get(child_id).schema(&ctx.arena).columns.clone();
            let mut col_ndvs = std::collections::HashMap::new();
            for (i, c) in child_cols.iter().enumerate() {
                col_ndvs.insert(c.unique_id, (10 * (i + 1)) as f64);
            }
            let group_ndvs: Vec<GroupNDV> = spec["group_ndvs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| GroupNDV {
                            cols: g
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|i| child_cols[i.as_u64().unwrap() as usize].unique_id)
                                .collect(),
                            ndv: 42.0,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let child_stats = Arc::new(StatsInfo {
                row_count: 1000.0,
                col_ndvs,
                group_ndvs,
                ..Default::default()
            });
            let col_groups: Vec<Vec<Arc<Column>>> = spec["col_groups"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| {
                            g.as_array()
                                .unwrap()
                                .iter()
                                .map(|i| Arc::clone(&child_cols[i.as_u64().unwrap() as usize]))
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (s1, s2, input_count) = with_detached(&mut ctx, agg_id, |node, ctx| {
                let s1 = node.derive_stats(ctx, agg_id, std::slice::from_ref(&child_stats), &[])?;
                let s2 = node.derive_stats(
                    ctx,
                    agg_id,
                    std::slice::from_ref(&child_stats),
                    &col_groups,
                )?;
                let input_count = node
                    .as_any()
                    .downcast_ref::<LogicalAggregation>()
                    .unwrap()
                    .input_count;
                Ok::<_, Error>((s1, s2, input_count))
            })
            .unwrap();
            let mut uids: Vec<i64> = s2.col_ndvs.keys().copied().collect();
            uids.sort();
            let ndv_out: Vec<Value> = uids
                .iter()
                .map(|&k| json!([k, format_float(s2.col_ndvs[&k], b'g', -1, 64)]))
                .collect();
            let gndv_out = |gs: &[GroupNDV]| -> Vec<Value> {
                gs.iter()
                    .map(|g| json!([g.cols, format_float(g.ndv, b'g', -1, 64)]))
                    .collect()
            };
            Outcome::Value(json!({
                "row_count": format_float(s2.row_count, b'g', -1, 64),
                "input_count": format_float(input_count, b'g', -1, 64),
                "col_ndvs": ndv_out,
                "gndvs1": gndv_out(&s1.group_ndvs),
                "gndvs2": gndv_out(&s2.group_ndvs),
            }))
        }
        "props" => {
            let child_id = ctx.arena.get(agg_id).children()[0];
            let child_cols = ctx.arena.get(child_id).schema(&ctx.arena).columns.clone();
            let child_props: Vec<Vec<Arc<Column>>> = spec["child_props"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| {
                            g.as_array()
                                .unwrap()
                                .iter()
                                .map(|i| Arc::clone(&child_cols[i.as_u64().unwrap() as usize]))
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (ret, possible) = with_detached(&mut ctx, agg_id, |node, ctx| {
                let ret = node.prepare_possible_properties(&ctx.arena, vec![child_props]);
                let possible = node
                    .as_any()
                    .downcast_ref::<LogicalAggregation>()
                    .unwrap()
                    .possible_properties
                    .clone();
                (ret, possible)
            });
            let dump_props = |ps: &[Vec<Arc<Column>>]| -> Vec<Value> {
                ps.iter()
                    .map(|p| Value::Array(p.iter().map(|c| json!(c.unique_id)).collect()))
                    .collect()
            };
            Outcome::Value(json!({
                "ret": dump_props(&ret),
                "possible": dump_props(&possible),
            }))
        }
        other => panic!("unknown aggop kind {other}"),
    }
}

fn find_window(ctx: &PlanCtx<'_>, p: PlanId) -> Option<PlanId> {
    let node = ctx.arena.get(p);
    if node.kind() == LogicalKind::Window {
        return Some(p);
    }
    node.children().iter().find_map(|&c| find_window(ctx, c))
}

fn run_winop(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    collate::set_new_collation_enabled_for_test(true);
    let sql = args[1].as_str().unwrap();
    let spec = &args[2];
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.sql_mode = mode;
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);
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
            return Outcome::Value(err_out(&e));
        }
        node.get_resolve_context()
    };
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

    let wh = Arc::new(new_static_warn_handler(0));
    let ev = es::new_eval_context(vec![
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
    ctx.client = Some(Arc::new(MockClient));
    let lp = match builder.build(&mut ctx, &stmt) {
        Ok(p) => p,
        Err(e) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(err_out(&e));
        }
    };
    let win_id = find_window(&ctx, lp)
        .unwrap_or_else(|| panic!("no LogicalWindow in {sql} ({spec}): {}", dump(&ctx, lp)));
    let kind = spec["kind"].as_str().unwrap();
    match kind {
        "inspect" => {
            let win = ctx.arena.downcast_ref::<LogicalWindow>(win_id).unwrap();
            let pby: Vec<Value> = win
                .get_partition_by()
                .iter()
                .map(|it| json!([it.col.unique_id, it.desc]))
                .collect();
            let pby_cols: Vec<Value> = win
                .get_partition_by_cols()
                .iter()
                .map(|c| json!(c.unique_id))
                .collect();
            let part_keys: Vec<Value> = win
                .get_partition_keys()
                .iter()
                .map(|k| json!([k.col.unique_id, k.collate_id]))
                .collect();
            let res_cols: Vec<Value> = win
                .get_window_result_columns(&ctx.arena)
                .iter()
                .map(|c| json!(c.unique_id))
                .collect();
            let cor_cols: Vec<Value> = win
                .extract_correlated_cols()
                .iter()
                .map(|c| json!(c.column.unique_id))
                .collect();
            let child_id = win.children()[0];
            let child_cols = ctx.arena.get(child_id).schema(&ctx.arena).columns.clone();
            let col_groups_in: Vec<Vec<Arc<Column>>> = spec["col_groups"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| {
                            g.as_array()
                                .unwrap()
                                .iter()
                                .map(|i| Arc::clone(&child_cols[i.as_u64().unwrap() as usize]))
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let col_groups_out: Vec<Value> = win
                .extract_col_groups(&ctx.arena, &col_groups_in)
                .iter()
                .map(|g| Value::Array(g.iter().map(|c| json!(c.unique_id)).collect()))
                .collect();
            Outcome::Value(json!({
                "pby": pby,
                "pby_cols": pby_cols,
                "part_keys": part_keys,
                "res_cols": res_cols,
                "cor_cols": cor_cols,
                "col_groups": col_groups_out,
            }))
        }
        "frame" => {
            let win = ctx.arena.downcast_ref::<LogicalWindow>(win_id).unwrap();
            let Some(fr) = &win.frame else {
                return Outcome::Value(json!({"frame": Value::Null}));
            };
            let mut cloned = fr.clone_frame();
            let order_by_cols: Vec<Arc<Column>> =
                win.order_by.iter().map(|it| Arc::clone(&it.col)).collect();
            let bctx = tidb_sessionctx::planctx::BuildPBContext {
                expr_ctx: Arc::clone(&ctx.expr_ctx) as tidb_sessionctx::exprctx::BuildContextRef,
                client: Arc::new(MockClient),
                tiflash_fast_scan: ctx.vars.tiflash_fast_scan,
                tiflash_fine_grained_shuffle_batch_size: ctx
                    .vars
                    .tiflash_fine_grained_shuffle_batch_size,
                group_concat_max_len: ctx.vars.group_concat_max_len,
                in_explain_stmt: false,
                warn_handler: None,
                extra_warnghandler: None,
            };
            let ectx = ctx.eval_ctx();
            let dump_bound_op = |b: Option<&mut FrameBound>| -> Result<Value, Error> {
                let Some(b) = b else {
                    return Ok(Value::Null);
                };
                b.update_compare_cols(&*ctx.expr_ctx, &order_by_cols)?;
                let tiflash_ok = win.check_comparison_for_tiflash(&ctx, b);
                let pb = b.to_pb(&bctx)?;
                let pb_range = if pb.has_frame_range() {
                    let r = pb.get_frame_range();
                    json!([r.get_tp() as i32, r.get_sig() as i32])
                } else {
                    Value::Null
                };
                let pb_cmp = if pb.has_cmp_data_type() {
                    json!(pb.get_cmp_data_type() as i32)
                } else {
                    Value::Null
                };
                let pb_off = if pb.has_offset() {
                    json!(pb.get_offset())
                } else {
                    Value::Null
                };
                Ok(json!({
                    "cmp_cols": exprs_out(&*ectx, &b.compare_cols),
                    "cmp_data_type": b.cmp_data_type as i32,
                    "tiflash_ok": tiflash_ok,
                    "pb": [
                        pb.get_type() as i32,
                        pb.get_unbounded(),
                        pb_off,
                        pb_cmp,
                        pb_range,
                    ],
                }))
            };
            let st = match dump_bound_op(cloned.start.as_mut()) {
                Ok(v) => v,
                Err(e) => {
                    if let Some(b) = blocked_by(&e) {
                        return Outcome::Blocked(b.to_string());
                    }
                    return Outcome::Value(err_out(&e));
                }
            };
            let en = match dump_bound_op(cloned.end.as_mut()) {
                Ok(v) => v,
                Err(e) => {
                    if let Some(b) = blocked_by(&e) {
                        return Outcome::Blocked(b.to_string());
                    }
                    return Outcome::Value(err_out(&e));
                }
            };
            Outcome::Value(json!({
                "type": cloned.tp as i32,
                "start": st,
                "end": en,
            }))
        }
        "replace" => {
            let child_id = ctx.arena.get(win_id).children()[0];
            let old_col = Arc::clone(&ctx.arena.get(child_id).schema(&ctx.arena).columns[0]);
            let mut new_col = (*old_col).clone();
            new_col.unique_id = 9999;
            let mut replace = std::collections::HashMap::new();
            replace.insert(old_col.hash_code().to_vec(), Arc::new(new_col));
            ctx.arena.get_mut(win_id).replace_expr_columns(&replace);
            Outcome::Value(json!({"plan": dump(&ctx, win_id)}))
        }
        "ppd" => {
            let opt = default_logical_optimize_option();
            let win_cols = ctx.arena.get(win_id).schema(&ctx.arena).columns.clone();
            let mut preds = Vec::new();
            if let Some(arr) = spec["pred_cols"].as_array() {
                for i in arr {
                    let col = Arc::clone(&win_cols[i.as_u64().unwrap() as usize]);
                    let pred = tidb_expression::scalar_function::new_function(
                        &*ctx.expr_ctx,
                        "gt",
                        tidb_types::field_type::new_field_type(tidb_base::mysql::TypeTiny),
                        vec![
                            Arc::new(Expr::Column(Column::clone(&col))),
                            Arc::new(Expr::Constant(tidb_expression::new_one())),
                        ],
                    )
                    .unwrap();
                    preds.push(pred);
                }
            }
            match pbase::predicate_push_down(&mut ctx, win_id, preds, &opt) {
                Ok((rem, new_p)) => {
                    let ectx = ctx.eval_ctx();
                    let rem_val = exprs_out(&*ectx, &rem);
                    drop(ectx);
                    Outcome::Value(json!({"plan": dump(&ctx, new_p), "rem": rem_val}))
                }
                Err(e) => Outcome::Value(err_out(&e)),
            }
        }
        "prune" => {
            let opt = default_logical_optimize_option();
            let used: Vec<Arc<Column>> = spec["keep"]
                .as_array()
                .map(|arr| {
                    let cols = &ctx.arena.get(lp).schema(&ctx.arena).columns;
                    arr.iter()
                        .map(|i| Arc::clone(&cols[i.as_u64().unwrap() as usize]))
                        .collect()
                })
                .unwrap_or_default();
            match pbase::prune_columns(&mut ctx, lp, used, &opt) {
                Ok(new_p) => Outcome::Value(json!({"plan": dump(&ctx, new_p)})),
                Err(e) => Outcome::Value(err_out(&e)),
            }
        }
        "stats" => {
            let child_id = ctx.arena.get(win_id).children()[0];
            let child_cols = ctx.arena.get(child_id).schema(&ctx.arena).columns.clone();
            let mut col_ndvs = std::collections::HashMap::new();
            for (i, c) in child_cols.iter().enumerate() {
                col_ndvs.insert(c.unique_id, (10 * (i + 1)) as f64);
            }
            let group_ndvs: Vec<GroupNDV> = spec["group_ndvs"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| GroupNDV {
                            cols: g
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|i| child_cols[i.as_u64().unwrap() as usize].unique_id)
                                .collect(),
                            ndv: 42.0,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let child_stats = Arc::new(StatsInfo {
                row_count: 1000.0,
                col_ndvs,
                group_ndvs,
                ..Default::default()
            });
            let col_groups: Vec<Vec<Arc<Column>>> = spec["col_groups"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|g| {
                            g.as_array()
                                .unwrap()
                                .iter()
                                .map(|i| Arc::clone(&child_cols[i.as_u64().unwrap() as usize]))
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (s1, s2) = with_detached(&mut ctx, win_id, |node, ctx| {
                let s1 = node.derive_stats(ctx, win_id, std::slice::from_ref(&child_stats), &[])?;
                let s2 = node.derive_stats(
                    ctx,
                    win_id,
                    std::slice::from_ref(&child_stats),
                    &col_groups,
                )?;
                Ok::<_, Error>((s1, s2))
            })
            .unwrap();
            let mut uids: Vec<i64> = s2.col_ndvs.keys().copied().collect();
            uids.sort();
            let ndv_out: Vec<Value> = uids
                .iter()
                .map(|&k| json!([k, format_float(s2.col_ndvs[&k], b'g', -1, 64)]))
                .collect();
            let gndv_out = |gs: &[GroupNDV]| -> Vec<Value> {
                gs.iter()
                    .map(|g| json!([g.cols, format_float(g.ndv, b'g', -1, 64)]))
                    .collect()
            };
            Outcome::Value(json!({
                "row_count": format_float(s2.row_count, b'g', -1, 64),
                "col_ndvs": ndv_out,
                "gndvs1": gndv_out(&s1.group_ndvs),
                "gndvs2": gndv_out(&s2.group_ndvs),
            }))
        }
        "props" => {
            let ret = with_detached(&mut ctx, win_id, |node, ctx| {
                node.prepare_possible_properties(&ctx.arena, Vec::new())
            });
            let res: Vec<Value> = ret
                .iter()
                .map(|p| Value::Array(p.iter().map(|c| json!(c.unique_id)).collect()))
                .collect();
            Outcome::Value(json!({"ret": res}))
        }
        other => panic!("unknown winop kind {other}"),
    }
}

/// The first LogicalJoin of the plan, with the Selection right above it (the ON conditions of
/// an inner join), like the Go domain's `sbFindJoin`.
fn find_join(ctx: &PlanCtx<'_>, p: PlanId) -> Option<(PlanId, Option<PlanId>)> {
    let node = ctx.arena.get(p);
    if node.kind() == LogicalKind::Selection {
        let c = node.children()[0];
        if ctx.arena.get(c).kind() == LogicalKind::Join {
            return Some((c, Some(p)));
        }
    }
    if node.kind() == LogicalKind::Join {
        return Some((p, None));
    }
    node.children().iter().find_map(|&c| find_join(ctx, c))
}

const ALL_JOIN_TYPES: [JoinType; 7] = [
    JoinType::InnerJoin,
    JoinType::LeftOuterJoin,
    JoinType::RightOuterJoin,
    JoinType::SemiJoin,
    JoinType::AntiSemiJoin,
    JoinType::LeftOuterSemiJoin,
    JoinType::AntiLeftOuterSemiJoin,
];

fn run_joinop(tables: &Arc<Tables>, args: &[Value]) -> Outcome {
    collate::set_new_collation_enabled_for_test(true);
    let sql = args[1].as_str().unwrap();
    let spec = &args[2];
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let mode = tidb_base::mysql::ModeOnlyFullGroupBy.0;
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_owned();
    vars.sql_mode = mode;
    let is = InfoSchemaHandle(Arc::clone(tables) as Arc<dyn InfoSchemaLookup>);
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
            return Outcome::Value(err_out(&e));
        }
        node.get_resolve_context()
    };
    vars.state.plan_id.0.store(0, Ordering::SeqCst);
    vars.state.plan_column_id.0.store(0, Ordering::SeqCst);
    let mut builder = new_plan_builder(&[]);
    builder.init(&mut vars, Some(is), None);
    builder.sql_text = sql;
    builder.resolve_ctx = Some(resolve_ctx);

    let wh = Arc::new(new_static_warn_handler(0));
    let ev = es::new_eval_context(vec![
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
    ctx.client = Some(Arc::new(MockClient));
    let lp = match builder.build(&mut ctx, &stmt) {
        Ok(p) => p,
        Err(e) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            return Outcome::Value(err_out(&e));
        }
    };
    let (join_id, sel_id) = find_join(&ctx, lp)
        .unwrap_or_else(|| panic!("no LogicalJoin in {sql} ({spec}): {}", dump(&ctx, lp)));
    let conds = sel_id.map_or_else(Vec::new, |s| {
        ctx.arena
            .downcast_ref::<LogicalSelection>(s)
            .unwrap()
            .conditions
            .clone()
    });
    let (left_id, right_id) = {
        let ch = ctx.arena.get(join_id).children();
        (ch[0], ch[1])
    };
    let spec_join_type = spec["join_type"].as_i64().unwrap_or(0);
    let kind = spec["kind"].as_str().unwrap();
    match kind {
        "inspect" => {
            let ectx = ctx.eval_ctx();
            let ectx = &*ectx;
            let join = ctx.arena.downcast_ref::<LogicalJoin>(join_id).unwrap();
            let mut types = Vec::new();
            let mut schemas = Vec::new();
            for t in ALL_JOIN_TYPES {
                types.push(json!([t.string(), t.is_outer_join(), t.is_semi_join()]));
                let schema = build_logical_join_schema(t, join, &ctx.arena);
                schemas.push(cols_out(ectx, &schema.columns));
            }
            let uids = |cs: &[Arc<Column>]| -> Value {
                Value::Array(cs.iter().map(|c| json!(c.unique_id)).collect())
            };
            let (lk, rk, null_eq, has_null_eq) = join.get_join_keys();
            let (nlk, nrk) = join.get_na_join_keys();
            let prefer: Vec<Value> = spec["flags"]
                .as_array()
                .map(|fs| {
                    fs.iter()
                        .map(|f| json!(join.prefer_any(&[f.as_u64().unwrap()])))
                        .collect()
                })
                .unwrap_or_default();
            let (pl, pr) = join.get_potential_partition_keys();
            let pk = |ks: &[tidb_planner::property::MPPPartitionColumn]| -> Value {
                Value::Array(
                    ks.iter()
                        .map(|k| json!([k.col.unique_id, k.collate_id]))
                        .collect(),
                )
            };
            Outcome::Value(json!({
                "types": types,
                "keys": [uids(&lk), uids(&rk), null_eq, has_null_eq],
                "na_keys": [uids(&nlk), uids(&nrk)],
                "naaj": join.is_naaj(),
                "prefer_any": prefer,
                "part_keys": [pk(&pl), pk(&pr)],
                "schemas": schemas,
            }))
        }
        "keyinfo" => {
            for (child, field) in [(left_id, "left_keys"), (right_id, "right_keys")] {
                let cols = ctx.arena.get(child).schema(&ctx.arena).columns.clone();
                let keys: Vec<Vec<Arc<Column>>> = spec[field]
                    .as_array()
                    .map(|ks| {
                        ks.iter()
                            .map(|k| {
                                k.as_array()
                                    .unwrap()
                                    .iter()
                                    .map(|i| Arc::clone(&cols[i.as_u64().unwrap() as usize]))
                                    .collect()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ctx.arena.schema_mut(child).keys = keys;
            }
            let (keys, max_one_row) = with_detached(&mut ctx, join_id, |node, ctx| {
                let join = node.as_any_mut().downcast_mut::<LogicalJoin>().unwrap();
                if spec_join_type >= 0 {
                    join.join_type = ALL_JOIN_TYPES[spec_join_type as usize];
                }
                join.build_key_info(&mut ctx.arena);
                let keys: Vec<Value> = join
                    .schema(&ctx.arena)
                    .keys
                    .iter()
                    .map(|k| Value::Array(k.iter().map(|c| json!(c.unique_id)).collect()))
                    .collect();
                (keys, join.max_one_row())
            });
            Outcome::Value(json!({"keys": keys, "max_one_row": max_one_row}))
        }
        "attach" => {
            let res = with_detached(&mut ctx, join_id, |node, ctx| {
                let join = node.as_any_mut().downcast_mut::<LogicalJoin>().unwrap();
                join.join_type = ALL_JOIN_TYPES[spec_join_type as usize];
                join.attach_on_conds(ctx, &conds)
            });
            match res {
                Ok(()) => Outcome::Value(json!({"plan": dump(&ctx, join_id)})),
                Err(e) => match blocked_by(&e) {
                    Some(b) => Outcome::Blocked(b.to_string()),
                    None => Outcome::Value(err_out(&e)),
                },
            }
        }
        "extract" => {
            let derive_left = spec["derive_left"].as_bool().unwrap_or(false);
            let derive_right = spec["derive_right"].as_bool().unwrap_or(false);
            let left_schema = ctx.arena.get(left_id).schema(&ctx.arena).clone();
            let right_schema = ctx.arena.get(right_id).schema(&ctx.arena).clone();
            let res = with_detached(&mut ctx, join_id, |node, ctx| {
                let join = node.as_any_mut().downcast_mut::<LogicalJoin>().unwrap();
                join.extract_on_condition(
                    ctx,
                    &conds,
                    &left_schema,
                    &right_schema,
                    derive_left,
                    derive_right,
                )
            });
            match res {
                Ok((eq, l, r, o)) => {
                    let ectx = ctx.eval_ctx();
                    let ectx = &*ectx;
                    Outcome::Value(json!({
                        "eq": exprs_out(ectx, &eq),
                        "left": exprs_out(ectx, &l),
                        "right": exprs_out(ectx, &r),
                        "other": exprs_out(ectx, &o),
                    }))
                }
                Err(e) => match blocked_by(&e) {
                    Some(b) => Outcome::Blocked(b.to_string()),
                    None => Outcome::Value(err_out(&e)),
                },
            }
        }
        "hints" => {
            let left = spec["left"].as_u64().unwrap_or(0);
            let right = spec["right"].as_u64().unwrap_or(0);
            let prefer = with_detached(&mut ctx, join_id, |node, ctx| {
                let join = node.as_any_mut().downcast_mut::<LogicalJoin>().unwrap();
                join.left_prefer_join_type = left;
                join.right_prefer_join_type = right;
                join.set_preferred_join_type(ctx);
                join.prefer_join_type
            });
            let w = warns(ctx.vars, &wh);
            Outcome::Value(json!({"prefer": prefer, "w": w}))
        }
        "replace" => {
            let old_col = Arc::clone(&ctx.arena.get(left_id).schema(&ctx.arena).columns[0]);
            let mut new_col = (*old_col).clone();
            new_col.unique_id = 9999;
            let mut replace = std::collections::HashMap::new();
            replace.insert(old_col.hash_code().to_vec(), Arc::new(new_col));
            ctx.arena.get_mut(join_id).replace_expr_columns(&replace);
            Outcome::Value(json!({"plan": dump(&ctx, join_id)}))
        }
        other => panic!("joinop: unknown kind {other}"),
    }
}

/// Records in `selectbuild` whose `.vi` differs from Go v8.5.8 strictly because of owner-approved
/// ruling `R29` (`specs/D04.1-privilege/spec.md` §5.1–§5.2, §6.1 D1 option A): `tidb-rs` registers
/// table-read `SelectPriv` (`column = "*"`, error `1142`) at `build_data_source` and removes Go's
/// case-sensitive `collectPrivsForCount` patch. Pinned so any unexpected drift fails CI.
/// Includes the 2 records of `SELECT a FROM t2 USE INDEX (ia)`, which build now that the index
/// hint loop of `getPossibleAccessPaths` is ported, and read a base table like the others,
/// and the 165 join statements that build (join plan building part A, D09.3): one exact `R29`
/// entry per base table of the join.
const EXPECTED_R29_NORMALIZED: usize = 2488;

fn select_list_has_group_by(list: &tidb_parser::ast::SetOprSelectList) -> bool {
    list.selects.iter().any(|s| match s {
        tidb_parser::ast::SetOprNode::Select(sel) => sel.group_by.is_some(),
        tidb_parser::ast::SetOprNode::List(inner) => select_list_has_group_by(inner),
        tidb_parser::ast::SetOprNode::Unported(_) => false,
    })
}

/// Returns true if `stmt` (or any nested SELECT in a set operation) has a `GROUP BY` clause.
fn stmt_has_group_by(stmt: &tidb_parser::ast::Stmt) -> bool {
    match stmt {
        tidb_parser::ast::Stmt::Select(sel) => sel.group_by.is_some(),
        tidb_parser::ast::Stmt::SetOpr(set_opr) => select_list_has_group_by(&set_opr.select_list),
        _ => false,
    }
}

/// Checks whether `entry` is an R29 `GROUP BY` column-privilege `VisitInfo` (`SelectPriv`, 1143)
/// on one of the statement's resolved `(db, tbl)` pairs.
fn is_r29_gby_col_vi(entry: &Value, resolved_tables: &[(String, String)]) -> bool {
    let Some(arr) = entry.as_array() else {
        return false;
    };
    if arr.len() != 6 || arr[0] != json!(4) || arr[5] != json!(1143) {
        return false;
    }
    let (Some(db), Some(tbl), Some(col), Some(msg)) = (
        arr[1].as_str(),
        arr[2].as_str(),
        arr[3].as_str(),
        arr[4].as_str(),
    ) else {
        return false;
    };
    if col.is_empty() || col == "*" {
        return false;
    }
    resolved_tables.iter().any(|(d, t)| d == db && t == tbl)
        && msg
            == format!(
                "[planner:1143]SELECT command denied to user ''@'' for column '{col}' in table '{tbl}'"
            )
}

/// The resolved tables whose `_tidb_rowid` `sql` references (ruling R32): `t._tidb_rowid`
/// names table `t`; an unqualified `_tidb_rowid` names the statement's only table. Each of
/// them must register exactly its own R32 entry, and no other table may register one.
/// `None`: an unqualified `_tidb_rowid` among several tables, which the builder resolves (or
/// reports ambiguous); then any of the tables may register it, and at least one must.
fn r32_rowid_tables(
    sql: &str,
    resolved_tables: &[(String, String)],
) -> Option<Vec<(String, String)>> {
    let lower = sql.to_lowercase();
    let mut tables: Vec<(String, String)> = Vec::new();
    let mut add = |t: &(String, String)| {
        if !tables.contains(t) {
            tables.push(t.clone());
        }
    };
    for (pos, _) in lower.match_indices("_tidb_rowid") {
        let before = lower[..pos].trim_end_matches('`');
        if let Some(qual) = before.strip_suffix('.') {
            let qual = qual.trim_end_matches('`');
            let start = qual
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .map_or(0, |i| i + 1);
            let name = &qual[start..];
            for t in resolved_tables.iter().filter(|(_, t)| t == name) {
                add(t);
            }
        } else if let [only] = resolved_tables {
            add(only);
        } else {
            return None;
        }
    }
    Some(tables)
}

/// Checks whether `got` equals Go's `out` once the exact `R29` table-read `("*", 1142)` entries
/// for the statement's resolved base tables (including those folded into `TableDual` by
/// `WHERE 0` / `LIMIT 0`, per specs/D04.1-privilege/spec.md §5.1), `R29` `GROUP BY` column `1143`
/// entries (emitted by `rewrite_gby_exprs` where Go v8.5.8 mistakenly guarded `resolveGbyExprs`),
/// Go's superseded `collectPrivsForCount` `"*"` entries, and `R32` `_tidb_rowid` full-table
/// `VisitInfo` entries are accounted for. Returns `true` only when every resolved base table
/// actually emitted its exact `R29` entry in `got["vi"]` and every other field (`plan`,
/// `plan_id`, `col_id`, `opt_flag`, remaining `vi`, `w`) matches Go byte-for-byte.
fn matches_with_r29_vi_normalization(
    sql: &str,
    out: &Value,
    got: &Value,
    resolved_tables: &[(String, String)],
    has_group_by: bool,
) -> bool {
    let (Some(out_obj), Some(got_obj)) = (out.as_object(), got.as_object()) else {
        return false;
    };
    let (Some(out_vi), Some(got_vi)) = (
        out_obj.get("vi").and_then(Value::as_array),
        got_obj.get("vi").and_then(Value::as_array),
    ) else {
        return false;
    };
    if resolved_tables.is_empty() {
        return false;
    }

    // Every resolved table in the statement must have registered its exact R29 table-read
    // VisitInfo in `got["vi"]` (even if WHERE 0 / LIMIT 0 later folded DataSource to TableDual).
    let mut expected_entries: Vec<Value> = resolved_tables
        .iter()
        .map(|(db, tbl)| {
            json!([
                4,
                db,
                tbl,
                "*",
                format!("[planner:1142]SELECT command denied to user ''@'' for table '{tbl}'"),
                1142
            ])
        })
        .collect();

    // For single-table plans, the R29 entry must be the very first VisitInfo (ensuring R30
    // table-denied 1142 precedes column-denied 1143).
    if expected_entries.len() == 1 && got_vi.first() != Some(&expected_entries[0]) {
        return false;
    }

    // R32: `_tidb_rowid` references now register full-table SelectPriv (`column = ""`, 1143)
    // where Go v8.5.8 emitted none. The entry must name the table whose `_tidb_rowid` the
    // statement references (in a join, the qualified side), and every referenced table must
    // have one.
    let r32_tables = r32_rowid_tables(sql, resolved_tables);
    let r32_rowid_entries: Vec<Value> = r32_tables
        .as_deref()
        .unwrap_or(resolved_tables)
        .iter()
        .map(|(db, tbl)| {
            json!([
                4,
                db,
                tbl,
                "",
                format!(
                    "[planner:1143]SELECT command denied to user ''@'' for column '_tidb_rowid' in table '{tbl}'"
                ),
                1143
            ])
        })
        .collect();

    let mut rem_got_vi = Vec::with_capacity(got_vi.len());
    let mut r32_seen = vec![false; r32_rowid_entries.len()];
    for entry in got_vi {
        if let Some(pos) = expected_entries.iter().position(|e| e == entry) {
            expected_entries.remove(pos);
        } else if let Some(pos) = r32_rowid_entries.iter().position(|e| e == entry) {
            // R32 `_tidb_rowid` entry (not emitted by Go v8.5.8).
            r32_seen[pos] = true;
        } else {
            rem_got_vi.push(entry.clone());
        }
    }
    let r32_ok = if r32_tables.is_some() {
        !r32_seen.contains(&false)
    } else {
        r32_seen.contains(&true)
    };
    if !expected_entries.is_empty() || !r32_ok {
        return false;
    }

    // Go's superseded `collectPrivsForCount` emitted trailing `("*", 1142)` entries for lowercase
    // `count(*)`; drop only exact matches for the same resolved `(db, tbl)` pairs from Go's `vi`.
    let ds_star_entries: Vec<Value> = resolved_tables
        .iter()
        .map(|(db, tbl)| {
            json!([
                4,
                db,
                tbl,
                "*",
                format!("[planner:1142]SELECT command denied to user ''@'' for table '{tbl}'"),
                1142
            ])
        })
        .collect();
    let rem_out_vi: Vec<Value> = out_vi
        .iter()
        .filter(|entry| !ds_star_entries.contains(entry))
        .cloned()
        .collect();

    let norm_got_vi = if has_group_by && rem_got_vi != rem_out_vi {
        // R29 (`rewrite_gby_exprs`): Go v8.5.8 omitted `checkColPriv` during GROUP BY expression
        // rewriting, so `tidb-rs` emits additional `(SelectPriv, db, tbl, col, 1143)` entries for
        // GROUP BY column references while preserving every Go `VisitInfo` entry in exact order.
        let mut matched = Vec::with_capacity(rem_out_vi.len());
        let mut out_idx = 0usize;
        for entry in &rem_got_vi {
            if out_idx < rem_out_vi.len() && entry == &rem_out_vi[out_idx] {
                matched.push(entry.clone());
                out_idx += 1;
            } else if !is_r29_gby_col_vi(entry, resolved_tables) {
                return false;
            }
        }
        if out_idx != rem_out_vi.len() {
            return false;
        }
        matched
    } else {
        rem_got_vi
    };

    let mut norm_out = out_obj.clone();
    let mut norm_got = got_obj.clone();
    norm_out.insert("vi".into(), Value::Array(rem_out_vi));
    norm_got.insert("vi".into(), Value::Array(norm_got_vi));
    norm_out == norm_got
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
    let (mut records, mut equal, mut r29_normalized, mut errors_equal) =
        (0usize, 0usize, 0usize, 0usize);
    let mut blocked: BTreeMap<String, usize> = BTreeMap::new();
    let (mut diffs, mut unparsed) = (Vec::new(), Vec::new());
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        assert!(rec["err"].is_null(), "Go error in record: {rec}");
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
        let op = rec["op"].as_str().unwrap();
        let sql = args[1].as_str().unwrap();
        let outcome = match op {
            "build" => run(
                tables.as_ref().expect("the tables record comes first"),
                args,
            ),
            "aggop" => run_aggop(
                tables.as_ref().expect("the tables record comes first"),
                args,
            ),
            "winop" => run_winop(
                tables.as_ref().expect("the tables record comes first"),
                args,
            ),
            "joinop" => run_joinop(
                tables.as_ref().expect("the tables record comes first"),
                args,
            ),
            other => panic!("unknown op {other}"),
        };
        match outcome {
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
            Outcome::Value(mut got) => {
                let has_group_by = got
                    .as_object_mut()
                    .and_then(|o| o.remove("__r29_has_gby"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let resolved_tables: Vec<(String, String)> = got
                    .as_object_mut()
                    .and_then(|o| o.remove("__r29_tables"))
                    .and_then(|v| v.as_array().cloned())
                    .map(|arr| {
                        arr.into_iter()
                            .map(|pair| {
                                (
                                    pair[0].as_str().unwrap().to_owned(),
                                    pair[1].as_str().unwrap().to_owned(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
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
                } else if matches_with_r29_vi_normalization(
                    sql,
                    out,
                    &got,
                    &resolved_tables,
                    has_group_by,
                ) {
                    r29_normalized += 1;
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
        "selectbuild: {records} records, {equal} plans + {r29_normalized} R29-normalized plans and {errors_equal} errors equal to Go, blocked {blocked:?}, {} not parsed, {} differences",
        unparsed.len(),
        diffs.len()
    );
    for d in diffs.iter().take(20) {
        eprintln!("{d}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
    assert_eq!(
        r29_normalized, EXPECTED_R29_NORMALIZED,
        "R29-normalized record count changed"
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
