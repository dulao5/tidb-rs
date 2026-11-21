//! Replay of the govec domain `physops` (tracker `govec/domains/physops.go`): the pure
//! helpers of the physical operators and tasks, and `exhaustPhysicalPlans` of the logical
//! Limit, TopN, Sort, Selection and Projection over a DataSource or a TableDual, compared
//! with Go v8.5.8's results (`specs/D10.1-physical-plans/spec.md` §6).

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::gostd::strconv::format_float;
use tidb_base::kv;
use tidb_base::mysql::TypeLonglong;
use tidb_expression::{Column, Expr, ExprRef, Schema, new_schema};
use tidb_kv::checker::RequestTypeSupportedChecker;
use tidb_kv::kv::{Client, ClientSendOption, Request, Response};
use tidb_model::{ColumnInfo, TableCacheStatusEnable, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_planner::base::exhaust_physical_plans;
use tidb_planner::core::exhaust_physical_plans::match_items;
use tidb_planner::core::physical_plans::{
    NominalSort, PhysicalLimit, PhysicalProjection, PhysicalSelection, PhysicalSort,
    PhysicalTableScan, PhysicalTopN, ReadReqType, expand_virtual_column,
};
use tidb_planner::core::task::{calc_paging_cost, get_avg_row_size};
use tidb_planner::logicalop::{
    DataSource, LogicalLimit, LogicalProjection, LogicalSelection, LogicalSort, LogicalTableDual,
    LogicalTopN,
};
use tidb_planner::property::{PartialOrderInfo, PhysicalProperty, SortItem, StatsInfo, TaskType};
use tidb_planner::util::{AccessPath, ByItems};
use tidb_planner::{LogicalPlan, PhysicalPlan, PhysicalPlanRef, PlanCtx, PlanId};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_types::field_type::new_field_type;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/physops.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
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

/// Go's `mock.Client`: every request type `kv.RequestTypeSupportedChecker` accepts.
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

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)`.
fn f(x: f64) -> String {
    format_float(x, b'g', -1, 64)
}

fn col(uid: i64, id: i64) -> Arc<Column> {
    Arc::new(Column {
        unique_id: uid,
        id,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..Column::default()
    })
}

fn col_expr(uid: i64) -> ExprRef {
    Arc::new(Expr::Column((*col(uid, uid)).clone()))
}

fn one() -> ExprRef {
    Arc::new(Expr::Constant(tidb_expression::constant::new_one()))
}

fn sort_items(v: &Value) -> Vec<SortItem> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|it| {
                    let uid = it[0].as_i64().unwrap();
                    SortItem {
                        col: col(uid, uid),
                        desc: it[1].as_bool().unwrap(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn by_items(v: &Value) -> Vec<ByItems> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|it| ByItems {
                    expr: if it[0] == "k" {
                        one()
                    } else {
                        col_expr(it[1].as_i64().unwrap())
                    },
                    desc: it[2].as_bool().unwrap(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn stats(rows: f64) -> Option<Arc<StatsInfo>> {
    Some(Arc::new(StatsInfo {
        row_count: rows,
        ..Default::default()
    }))
}

fn expand(args: &[Value]) -> Value {
    let deps: std::collections::HashMap<i64, i64> = args[3]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v[0].as_i64().unwrap(), v[1].as_i64().unwrap()))
        .collect();
    fn mk(deps: &std::collections::HashMap<i64, i64>, id: i64, uid: i64) -> Arc<Column> {
        let mut c = (*col(uid, id)).clone();
        if let Some(&d) = deps.get(&id) {
            c.virtual_expr = Some(Arc::new(Expr::Column((*mk(deps, d, 1000 + d)).clone())));
        }
        Arc::new(c)
    }
    let mut schema = new_schema(
        args[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| mk(&deps, s[0].as_i64().unwrap(), s[1].as_i64().unwrap()))
            .collect(),
    );
    let info = |v: &Value| -> Vec<Arc<ColumnInfo>> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|id| {
                Arc::new(ColumnInfo {
                    id: id.as_i64().unwrap(),
                    ..Default::default()
                })
            })
            .collect()
    };
    let cols = expand_virtual_column(&info(&args[1]), &mut schema, &info(&args[2]));
    json!({
        "cols": cols.iter().map(|c| c.id).collect::<Vec<_>>(),
        "schema": schema.columns.iter().map(|c| vec![c.id, c.unique_id]).collect::<Vec<_>>(),
    })
}

fn new_ctx(vars: &mut SessionVars) -> PlanCtx<'_> {
    let mut ctx = PlanCtx::new(vars, es::new_expr_context(Vec::new()));
    ctx.client = Some(Arc::new(MockClient));
    ctx
}

fn paging(args: &[Value]) -> Value {
    let index_rows: f64 = args[0].as_str().unwrap().parse().unwrap();
    let mut vars = SessionVars::new(Box::new(Acc));
    if let Some(v) = args[3].as_str().filter(|v| !v.is_empty()) {
        vars.seek_factor = v.parse().unwrap();
    }
    if let Some(v) = args[4].as_str().filter(|v| !v.is_empty()) {
        vars.cpu_factor = v.parse().unwrap();
    }
    let ctx = new_ctx(&mut vars);
    let mut children: Vec<PhysicalPlanRef> = Vec::new();
    for r in args[1].as_array().unwrap() {
        let mut ts = PhysicalTableScan::default().init(&ctx, 0);
        ts.set_stats(stats(r.as_str().unwrap().parse().unwrap()));
        children.push(Arc::new(ts));
    }
    let mut sel = PhysicalSelection::default().init(&ctx, stats(index_rows), 0, Vec::new());
    sel.set_children(children);
    json!(f(calc_paging_cost(&ctx, &sel, args[2].as_u64().unwrap())))
}

fn avg_row_size(args: &[Value]) -> Value {
    let cols: Vec<Arc<Column>> = args[0]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, ft)| {
            let mut tp: FieldType = new_field_type(ft[0].as_u64().unwrap() as u8);
            tp.set_flag(ft[1].as_u64().unwrap() as _);
            tp.set_flen(ft[2].as_i64().unwrap() as isize);
            tp.set_decimal(ft[3].as_i64().unwrap() as isize);
            Arc::new(Column {
                unique_id: i as i64 + 1,
                ret_type: Arc::new(tp),
                ..Column::default()
            })
        })
        .collect();
    json!(f(get_avg_row_size(
        &StatsInfo {
            row_count: 1.0,
            ..Default::default()
        },
        &cols
    )))
}

fn data_source(ctx: &mut PlanCtx<'_>, child: &str) -> PlanId {
    let mut tbl = TableInfo {
        id: 100,
        ..Default::default()
    };
    if child == "ds_cached" {
        tbl.table_cache_status_type = TableCacheStatusEnable;
    }
    let mut ds = DataSource::init(
        ctx,
        Arc::new(tbl),
        CIStr::default(),
        Vec::new(),
        new_schema(vec![col(1, 1), col(2, 2), col(3, 3)]),
        0,
    );
    ds.possible_access_paths = vec![AccessPath {
        store_type: kv::TiKV,
        ..Default::default()
    }];
    if child == "ds_flash" {
        ds.possible_access_paths.push(AccessPath {
            store_type: kv::TiFlash,
            ..Default::default()
        });
    }
    ds.base.plan.set_stats(stats(1000.0));
    ctx.arena.alloc(Box::new(ds))
}

fn items_out(items: &[SortItem]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|s| json!([s.col.unique_id, s.desc]))
            .collect(),
    )
}

fn uid_of(e: &ExprRef) -> Value {
    match &**e {
        Expr::Column(c) => json!(c.unique_id),
        _ => Value::Null,
    }
}

fn by_out(by: &[ByItems]) -> Value {
    Value::Array(
        by.iter()
            .map(|b| json!([uid_of(&b.expr), b.desc]))
            .collect(),
    )
}

fn schema_out(s: &Schema) -> Value {
    Value::Array(s.columns.iter().map(|c| json!(c.unique_id)).collect())
}

fn prop_out(p: Option<&Arc<PhysicalProperty>>) -> Value {
    let Some(p) = p else {
        return Value::Null;
    };
    json!({
        "tp": p.task_tp.0, "exp": f(p.expected_cnt), "sort": items_out(&p.sort_items),
        "reject": p.reject_sort, "cte": p.cte_producer_status.0,
        "advisory": items_out(&p.advisory_sort_items),
        "part": p.partial_order_info.as_ref().map(|i| items_out(&i.sort_items)),
        "vec": p.vector_prop.vector_helper.is_some(),
    })
}

fn candidate(p: &PhysicalPlanRef) -> Value {
    let mut o = json!({
        "tp": p.tp(), "nominal": false, "rows": f(p.stats_info().unwrap().row_count),
        "child": prop_out(p.get_child_req_props(0)),
    });
    let a = p.as_any();
    if let Some(x) = a.downcast_ref::<PhysicalLimit>() {
        o["offset"] = json!(x.offset);
        o["count"] = json!(x.count);
        o["part_by"] = json!(x.get_partition_by().len());
        o["schema"] = schema_out(x.schema());
    } else if let Some(x) = a.downcast_ref::<PhysicalTopN>() {
        o["offset"] = json!(x.offset);
        o["count"] = json!(x.count);
        o["by"] = by_out(&x.by_items);
    } else if let Some(x) = a.downcast_ref::<PhysicalSort>() {
        o["by"] = by_out(&x.by_items);
    } else if let Some(x) = a.downcast_ref::<NominalSort>() {
        o["nominal"] = json!(true);
        o["only"] = json!(x.only_column);
        o["by"] = by_out(&x.by_items);
    } else if let Some(x) = a.downcast_ref::<PhysicalSelection>() {
        o["conds"] = Value::Array(x.conditions.iter().map(uid_of).collect());
    } else if let Some(x) = a.downcast_ref::<PhysicalProjection>() {
        o["exprs"] = Value::Array(x.exprs.iter().map(uid_of).collect());
        o["schema"] = schema_out(x.schema());
    }
    o
}

fn exhaust(c: &Value) -> Result<Value, Error> {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.allow_mpp_execution = c["mpp"].as_bool().unwrap();
    vars.limit_push_down_threshold = c["threshold"].as_i64().unwrap();
    if c["partial"].as_bool().unwrap() {
        vars.opt_partial_ordered_index_for_top_n = "COST".to_owned();
    }
    vars.allow_projection_push_down = c["proj_push"].as_bool().unwrap();
    let mut ctx = new_ctx(&mut vars);

    let child = match c["child"].as_str().unwrap() {
        "dual" => {
            let mut d = LogicalTableDual::init(&ctx, 0);
            d.sp.set_schema(Some(new_schema(vec![col(1, 1), col(2, 2), col(3, 3)])));
            d.base.plan.set_stats(stats(1.0));
            ctx.arena.alloc(Box::new(d))
        }
        "sel_ds" => {
            let ds = data_source(&mut ctx, "ds");
            let mut s = LogicalSelection::init(vec![col_expr(1)], &ctx, 0);
            s.set_children(vec![ds]);
            s.base.plan.set_stats(stats(500.0));
            ctx.arena.alloc(Box::new(s))
        }
        other => data_source(&mut ctx, other),
    };
    let offset = c["offset"].as_u64().unwrap();
    let count = c["count"].as_u64().unwrap();
    let prefer = c["prefer"].as_bool().unwrap();
    let mut lp: Box<dyn LogicalPlan> = match c["op"].as_str().unwrap() {
        "limit" => {
            let mut l = LogicalLimit::init(offset, count, &ctx, 0);
            l.prefer_limit_to_cop = prefer;
            Box::new(l)
        }
        "topn" => {
            let mut t = LogicalTopN::init(by_items(&c["by"]), offset, count, &ctx, 0);
            t.prefer_limit_to_cop = prefer;
            Box::new(t)
        }
        "sort" => Box::new(LogicalSort::init(by_items(&c["by"]), &ctx, 0)),
        "sel" => Box::new(LogicalSelection::init(
            by_items(&c["by"]).into_iter().map(|b| b.expr).collect(),
            &ctx,
            0,
        )),
        "proj" => {
            let mut exprs = Vec::new();
            let mut cols = Vec::new();
            for (i, e) in c["exprs"].as_array().unwrap().iter().enumerate() {
                exprs.push(if e[0] == "k" {
                    one()
                } else {
                    col_expr(e[1].as_i64().unwrap())
                });
                cols.push(col(100 + i as i64, 0));
            }
            let mut p = LogicalProjection::init(exprs, &ctx, 0);
            p.sp.set_schema(Some(new_schema(cols)));
            Box::new(p)
        }
        op => panic!("op {op}"),
    };
    lp.set_children(vec![child]);
    lp.base_mut()
        .plan
        .set_stats(stats(c["rows"].as_str().unwrap().parse().unwrap()));
    let id = ctx.arena.alloc(lp);
    let mut prop = PhysicalProperty {
        task_tp: TaskType(c["prop_tp"].as_i64().unwrap() as isize),
        sort_items: sort_items(&c["prop_sort"]),
        expected_cnt: c["prop_exp"].as_str().unwrap().parse().unwrap(),
        reject_sort: c["reject"].as_bool().unwrap(),
        ..Default::default()
    };
    if !c["prop_part"].is_null() {
        prop.partial_order_info = Some(Arc::new(PartialOrderInfo {
            sort_items: sort_items(&c["prop_part"]),
        }));
    }
    let (plans, hint) = exhaust_physical_plans(&mut ctx, id, &prop)?;
    let prefer_after = ctx
        .arena
        .downcast_ref::<LogicalLimit>(id)
        .map(|l| l.prefer_limit_to_cop)
        .or_else(|| {
            ctx.arena
                .downcast_ref::<LogicalTopN>(id)
                .map(|t| t.prefer_limit_to_cop)
        })
        .unwrap_or(false);
    let warnings: Vec<String> = ctx
        .vars
        .stmt_ctx()
        .get_warnings()
        .iter()
        .map(|w| w.err.to_string())
        .collect();
    Ok(json!({
        "plans": plans.iter().map(candidate).collect::<Vec<_>>(),
        "hint": hint,
        "prefer": prefer_after,
        "warnings": warnings,
    }))
}

fn replay(op: &str, args: &[Value]) -> Value {
    match op {
        "read_req" => json!(ReadReqType(args[0].as_u64().unwrap() as u8).name()),
        "expand" => expand(args),
        "match_items" => json!(match_items(
            &PhysicalProperty {
                sort_items: sort_items(&args[0]),
                ..Default::default()
            },
            &by_items(&args[1])
        )),
        "paging" => paging(args),
        "avg_row_size" => avg_row_size(args),
        "exhaust" => exhaust(&args[0]).expect("exhaust"),
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_physops() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "physops");
    let mut by_op: std::collections::BTreeMap<String, usize> = Default::default();
    let mut candidates = 0usize;
    let mut diffs = Vec::new();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        assert!(rec["err"].is_null(), "a Go error or panic record: {line}");
        let got = match catch_unwind(AssertUnwindSafe(|| replay(op, args))) {
            Ok(g) => g,
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                diffs.push(format!("{} {op} {args:?}: panic {msg}", rec["src"]));
                continue;
            }
        };
        if got == rec["out"] {
            *by_op.entry(op.to_owned()).or_default() += 1;
            if op == "exhaust" {
                candidates += got["plans"].as_array().unwrap().len();
            }
        } else {
            diffs.push(format!(
                "{} {op} {}:\n  go   {}\n  rust {got}",
                rec["src"],
                serde_json::to_string(args).unwrap(),
                rec["out"]
            ));
        }
    }
    eprintln!(
        "physops: equal to Go {by_op:?} ({candidates} exhaust candidates), {} differences",
        diffs.len()
    );
    for d in diffs.iter().take(10) {
        eprintln!("{d}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
}
