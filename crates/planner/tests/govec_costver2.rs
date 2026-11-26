//! Replay of the govec domain `costver2` (tracker `govec/domains/costver2.go`): the cost
//! model v2 of the operators of the first vertical slice (`getPlanCost`, `GetPlanCostVer2` of
//! Selection, Projection, TableScan, TableReader, Sort, TopN and the point get, the formula
//! helpers and the factor table), compared with Go v8.5.8's results
//! (`specs/D10.5-cost-model/spec.md` §6).

use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::gostd::strconv::format_float;
use tidb_base::kv::StoreType;
use tidb_base::mysql::{PriKeyFlag, TypeLonglong, UnsignedFlag};
use tidb_expression::scalar_function::new_function_internal;
use tidb_expression::{Column, Expr, ExprRef, new_schema};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, TableInfo, TempTableGlobal};
use tidb_parser::model::CIStr;
use tidb_parser::types::UnspecifiedLength;
use tidb_planner::baseimpl::new_base_plan;
use tidb_planner::core::physical_plans::{
    PhysPlanPartInfo, PhysicalIndexReader, PhysicalIndexScan, PhysicalLimit, PhysicalProjection,
    PhysicalSelection, PhysicalSort, PhysicalTableReader, PhysicalTableScan, PhysicalTopN,
};
use tidb_planner::core::plan_cost_ver1::get_tbl_stats;
use tidb_planner::core::plan_cost_ver2::{
    defaultVer2Factors, double_read_cost_ver2, filter_cost_ver2, get_number_of_ranges,
    get_plan_cost, get_task_cpu_factor_ver2, get_task_mem_factor_ver2, get_task_net_factor_ver2,
    get_task_request_factor_ver2, get_task_scan_factor_ver2, net_cost_ver2, order_cost_ver2,
    scan_cost_ver2,
};
use tidb_planner::core::point_get_plan::PointGetPlan;
use tidb_planner::property::{StatsInfo, TaskType};
use tidb_planner::util::ByItems;
use tidb_planner::util::costusage::{CostFlagTrace, CostVer2, CostVer2Factor};
use tidb_planner::util::optimizetrace::new_default_plan_cost_option;
use tidb_planner::{PhysicalKind, PhysicalPlan, PhysicalPlanRef, PlanCtx};
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::ranger::types::{Range, full_int_range, full_not_null_range, full_range};
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_sessionctx::variable::tidb_vars::EnableTmpStorageOnOOM;
use tidb_statistics::histogram::{new_histogram, new_stats_full_load_status};
use tidb_statistics::table::{HistColl, new_hist_coll_with_cols_and_idxs};
use tidb_types::collate::get_binary_collator_slice;
use tidb_types::datum::new_int_datum;
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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/costver2.jsonl.gz",
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

/// Go's `strconv.FormatFloat(f, 'g', -1, 64)`.
fn f(x: f64) -> String {
    format_float(x, b'g', -1, 64)
}

fn fl(v: &Value) -> f64 {
    v.as_str().unwrap().parse().unwrap()
}

fn cost_out(c: &CostVer2) -> Vec<Value> {
    match c.get_trace() {
        None => vec![json!(f(c.get_cost())), json!(""), Value::Null],
        Some(t) => {
            let factors: BTreeMap<String, String> = t
                .get_factor_costs()
                .iter()
                .map(|(k, v)| (k.clone(), f(*v)))
                .collect();
            vec![
                json!(f(c.get_cost())),
                json!(t.get_formula()),
                json!(factors),
            ]
        }
    }
}

fn column(uid: i64, tp: u8) -> Column {
    Column {
        unique_id: uid,
        id: uid,
        ret_type: Arc::new(new_field_type(tp)),
        ..Column::default()
    }
}

fn expr(ctx: &PlanCtx<'_>, kind: &str, col: &Column) -> ExprRef {
    let c: ExprRef = Arc::new(Expr::Column(col.clone()));
    match kind {
        "f" => new_function_internal(
            &*ctx.expr_ctx,
            "plus",
            new_field_type(TypeLonglong),
            vec![Arc::clone(&c), c],
        )
        .expect("plus"),
        "k" => Arc::new(Expr::Constant(tidb_expression::constant::new_one())),
        _ => c,
    }
}

fn range(kind: &str) -> Range {
    match kind {
        "int" => full_int_range(false).remove(0),
        "uint" => full_int_range(true).remove(0),
        "full" => full_range().remove(0),
        "notnull" => full_not_null_range().remove(0),
        _ => Range {
            low_val: vec![new_int_datum(5)],
            high_val: vec![new_int_datum(5)],
            collators: get_binary_collator_slice(1),
            low_exclude: false,
            high_exclude: false,
        },
    }
}

fn table(temp: bool, pk_handle: bool, unsigned: bool, cols: &[u8]) -> TableInfo {
    let mut tbl = TableInfo {
        id: 100,
        pk_is_handle: pk_handle,
        ..Default::default()
    };
    if temp {
        tbl.temp_table_type = TempTableGlobal;
    }
    let mut infos = Vec::new();
    for (i, tp) in cols.iter().enumerate() {
        let mut ci = ColumnInfo {
            id: i as i64 + 1,
            offset: i as isize,
            field_type: new_field_type(*tp),
            ..Default::default()
        };
        if i == 0 && pk_handle {
            ci.add_flag(PriKeyFlag);
            if unsigned {
                ci.add_flag(UnsignedFlag);
            }
        }
        infos.push(Arc::new(ci));
    }
    tbl.columns = Some(infos);
    tbl
}

fn hist_coll(h: &Value) -> HistColl {
    let count = h["count"].as_i64().unwrap();
    let mut cols = BTreeMap::new();
    if count >= 0 {
        let mut hg = new_histogram(
            1,
            count,
            h["null"].as_i64().unwrap(),
            0,
            Arc::new(new_field_type(TypeLonglong)),
            1,
            0,
        );
        if count > 0 {
            hg.append_bucket_with_ndv(&new_int_datum(1), &new_int_datum(100), count, 1, 0);
        }
        let mut c = tidb_statistics::Column::with_histogram(hg);
        c.stats_ver = 1;
        c.stats_loaded_status = new_stats_full_load_status();
        cols.insert(1, Arc::new(c));
    }
    let mut coll = new_hist_coll_with_cols_and_idxs(
        100,
        true,
        0,
        h["modify"].as_i64().unwrap(),
        cols,
        BTreeMap::new(),
    );
    coll.pseudo = h["pseudo"].as_bool().unwrap();
    coll
}

const FACTOR_VARS: [&str; 9] = [
    "table_full_scan",
    "table_range_scan",
    "table_rowid_scan",
    "table_tiflash_scan",
    "table_reader",
    "index_scan",
    "index_reader",
    "sort",
    "topn",
];

fn factor_var<'a>(vars: &'a mut SessionVars, name: &str) -> &'a mut f64 {
    match name {
        "table_full_scan" => &mut vars.table_full_scan_cost_factor,
        "table_range_scan" => &mut vars.table_range_scan_cost_factor,
        "table_rowid_scan" => &mut vars.table_row_id_scan_cost_factor,
        "table_tiflash_scan" => &mut vars.table_tiflash_scan_cost_factor,
        "table_reader" => &mut vars.table_reader_cost_factor,
        "index_scan" => &mut vars.index_scan_cost_factor,
        "index_reader" => &mut vars.index_reader_cost_factor,
        "sort" => &mut vars.sort_cost_factor,
        "topn" => &mut vars.topn_cost_factor,
        _ => unreachable!("{name}"),
    }
}

fn type_name(p: &dyn PhysicalPlan) -> &'static str {
    match p.kind() {
        PhysicalKind::PointGet => "PointGetPlan",
        PhysicalKind::TableReader => "PhysicalTableReader",
        PhysicalKind::TableScan => "PhysicalTableScan",
        PhysicalKind::IndexReader => "PhysicalIndexReader",
        PhysicalKind::IndexScan => "PhysicalIndexScan",
        PhysicalKind::Selection => "PhysicalSelection",
        PhysicalKind::Projection => "PhysicalProjection",
        PhysicalKind::Limit => "PhysicalLimit",
        PhysicalKind::Sort => "PhysicalSort",
        PhysicalKind::TopN => "PhysicalTopN",
        PhysicalKind::TableDual => "PhysicalTableDual",
        PhysicalKind::NominalSort => "NominalSort",
        _ => "other",
    }
}

fn collect(p: &dyn PhysicalPlan, out: &mut Vec<Value>) {
    let memo = p.base_physical().cost.lock().clone();
    let mut node = vec![json!(type_name(p)), json!(memo.plan_cost_init)];
    node.extend(cost_out(&memo.plan_cost_ver2));
    out.push(Value::Array(node));
    if let Some(r) = p.as_any().downcast_ref::<PhysicalTableReader>() {
        collect(r.table_plan.as_deref().unwrap(), out);
    }
    if let Some(r) = p.as_any().downcast_ref::<PhysicalIndexReader>()
        && let Some(ip) = r.index_plan.as_deref()
    {
        collect(ip, out);
    }
    for ch in p.children() {
        collect(ch.as_ref(), out);
    }
}

fn plan(c: &Value) -> Result<Value, Error> {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.enable_chunk_rpc = true;
    let v = &c["vars"];
    for (i, name) in FACTOR_VARS.iter().enumerate() {
        *factor_var(&mut vars, name) = fl(&v["factors"][i]);
    }
    let proj_conc = v["proj_conc"].as_i64().unwrap();
    if proj_conc >= 0 {
        vars.set_projection_concurrency(proj_conc);
    }
    vars.set_dist_sql_scan_concurrency(v["scan_conc"].as_i64().unwrap());
    vars.allow_mpp_execution = true;
    vars.enforce_mpp_execution = v["enforce_mpp"].as_bool().unwrap();
    vars.state.mem_tracker_bytes_limit = v["mem_quota"].as_i64().unwrap();
    EnableTmpStorageOnOOM.store(v["tmp_oom"].as_bool().unwrap(), SeqCst);
    vars.set_allow_prefer_range_scan(v["prefer_range"].as_bool().unwrap());
    if v["index_force"].as_bool().unwrap() {
        vars.stmt_ctx_mut().set_index_force();
    }
    vars.cost_model_version = v["version"].as_i64().unwrap();
    let mut ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));

    let mut reader_size: Option<String> = None;
    let root: PhysicalPlanRef = if c["point"].as_bool().unwrap() {
        let tbl = table(c["temp"].as_bool().unwrap(), false, false, &[TypeLonglong]);
        let cols: Vec<Arc<Column>> = c["point_cols"]
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, tp)| Arc::new(column(i as i64 + 1, tp.as_u64().unwrap() as u8)))
                    .collect()
            })
            .unwrap_or_default();
        let mut pg = PointGetPlan {
            tbl_info: Arc::new(tbl),
            schema: new_schema(cols.clone()),
            access_cols: cols,
            ..Default::default()
        };
        pg.base.plan = new_base_plan(&ctx, "Point_Get", 0);
        pg.set_stats(Some(Arc::new(StatsInfo {
            row_count: fl(&c["scan_rows"]),
            ..Default::default()
        })));
        Arc::new(pg)
    } else {
        let tps: Vec<u8> = c["cols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_u64().unwrap() as u8)
            .collect();
        let (pk_handle, unsigned) = (
            c["pk_handle"].as_bool().unwrap(),
            c["unsigned"].as_bool().unwrap(),
        );
        let tbl = table(c["temp"].as_bool().unwrap(), pk_handle, unsigned, &tps);
        let mut tbl_cols = Vec::new();
        for (i, tp) in tps.iter().enumerate() {
            let mut col = column(i as i64 + 1, *tp);
            if i == 0 && pk_handle {
                let mut ft = (*col.ret_type).clone();
                ft.add_flag(PriKeyFlag);
                if unsigned {
                    ft.add_flag(UnsignedFlag);
                }
                col.ret_type = Arc::new(ft);
            }
            tbl_cols.push(Arc::new(col));
        }
        let hist = Arc::new(hist_coll(&c["hist"]));
        let hist_stats = c["hist_stats"].as_bool().unwrap();
        let stats = |rows: &Value| {
            Some(Arc::new(StatsInfo {
                row_count: fl(rows),
                hist_coll: hist_stats.then(|| Arc::clone(&hist)),
                ..Default::default()
            }))
        };
        let ranges = c["ranges"]
            .as_array()
            .map(|a| a.iter().map(|r| range(r.as_str().unwrap())).collect())
            .unwrap_or_default();
        let store = StoreType(c["store"].as_u64().unwrap() as _);
        let schema_n = c["schema_n"].as_u64().unwrap() as usize;
        let is_index = c["index"].as_bool().unwrap_or(false);
        let mut cur: PhysicalPlanRef = if is_index {
            let idx_cols: Vec<IndexColumn> = (0..schema_n)
                .map(|i| IndexColumn {
                    offset: i as isize,
                    length: UnspecifiedLength,
                    ..Default::default()
                })
                .collect();
            let idx = Arc::new(IndexInfo {
                id: 1,
                name: CIStr::new("idx"),
                columns: Some(idx_cols),
                unique: c["unique"].as_bool().unwrap_or(false),
                ..Default::default()
            });
            let mut is = PhysicalIndexScan {
                table: Arc::new(tbl),
                index: idx,
                desc: c["desc"].as_bool().unwrap(),
                ranges,
                data_source_schema: Some(new_schema(tbl_cols[..schema_n].to_vec())),
                tbl_col_hists: Some(Arc::clone(&hist)),
                ..Default::default()
            }
            .init(&ctx, 0);
            is.set_schema(new_schema(tbl_cols[..schema_n].to_vec()));
            is.set_stats(stats(&c["scan_rows"]));
            Arc::new(is)
        } else {
            let mut ts = PhysicalTableScan {
                table: Arc::new(tbl),
                store_type: store,
                desc: c["desc"].as_bool().unwrap(),
                ranges,
                tbl_cols: tbl_cols.clone(),
                tbl_col_hists: Some(Arc::clone(&hist)),
                ..Default::default()
            }
            .init(&ctx, 0);
            ts.set_schema(new_schema(tbl_cols[..schema_n].to_vec()));
            ts.set_stats(stats(&c["scan_rows"]));
            ts.set_is_child_of_index_look_up(c["lookup"].as_bool().unwrap());
            if c["range_info"].as_bool().unwrap() {
                ts.range_info = "range decided by [eq(test.t.a, 1)]".to_owned();
            }
            if c["part_conds"].as_bool().unwrap() {
                ts.plan_part_info = Some(Arc::new(PhysPlanPartInfo {
                    pruning_conds: vec![Arc::new(Expr::Constant(
                        tidb_expression::constant::new_one(),
                    ))],
                    ..Default::default()
                }));
            }
            Arc::new(ts)
        };
        let build = |ctx: &mut PlanCtx<'_>, cur: PhysicalPlanRef, o: &Value| -> PhysicalPlanRef {
            let col = (*cur.schema().columns[0]).clone();
            let exprs: Vec<ExprRef> = o["exprs"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|e| expr(ctx, e.as_str().unwrap(), &col))
                        .collect()
                })
                .unwrap_or_default();
            let by: Vec<ByItems> = exprs
                .iter()
                .enumerate()
                .map(|(i, e)| ByItems {
                    expr: Arc::clone(e),
                    desc: i % 2 == 1,
                })
                .collect();
            let st = stats(&o["rows"]);
            let (count, offset) = (o["count"].as_u64().unwrap(), o["offset"].as_u64().unwrap());
            let mut next: Box<dyn PhysicalPlan> = match o["kind"].as_str().unwrap() {
                "sel" => Box::new(
                    PhysicalSelection {
                        conditions: exprs,
                        ..Default::default()
                    }
                    .init(ctx, st, 0, Vec::new()),
                ),
                "proj" => {
                    let n = exprs.len();
                    let mut p = PhysicalProjection {
                        exprs,
                        ..Default::default()
                    }
                    .init(ctx, st, 0, Vec::new());
                    p.set_schema(new_schema(
                        (0..n)
                            .map(|i| Arc::new(column(200 + i as i64, TypeLonglong)))
                            .collect(),
                    ));
                    Box::new(p)
                }
                "sort" => Box::new(
                    PhysicalSort {
                        by_items: by,
                        ..Default::default()
                    }
                    .init(ctx, st, 0, Vec::new()),
                ),
                "topn" => Box::new(
                    PhysicalTopN {
                        by_items: by,
                        count,
                        offset,
                        ..Default::default()
                    }
                    .init(ctx, st, 0, Vec::new()),
                ),
                "limit" => {
                    let mut l = PhysicalLimit {
                        count,
                        offset,
                        ..Default::default()
                    }
                    .init(ctx, st, 0, Vec::new());
                    l.set_schema(cur.schema().clone_schema());
                    Box::new(l)
                }
                k => unreachable!("{k}"),
            };
            next.set_children(vec![cur]);
            Arc::from(next)
        };
        for o in c["cop"].as_array().into_iter().flatten() {
            cur = build(&mut ctx, cur, o);
        }
        if c["reader"].as_bool().unwrap() {
            if is_index {
                let mut reader = PhysicalIndexReader {
                    index_plan: Some(cur),
                    ..Default::default()
                }
                .init(&ctx, 0);
                reader.set_stats(stats(&c["reader_rows"]));
                reader_size = Some(f(reader.get_net_data_size(ctx.vars)));
                cur = Arc::new(reader);
            } else {
                let mut reader = PhysicalTableReader {
                    table_plan: Some(cur),
                    store_type: store,
                    ..Default::default()
                }
                .init(&ctx, 0);
                reader.set_stats(stats(&c["reader_rows"]));
                reader_size = Some(f(reader.get_avg_row_size(&ctx)));
                cur = Arc::new(reader);
            }
        } else {
            let _ = get_tbl_stats(cur.as_ref());
        }
        for o in c["root"].as_array().into_iter().flatten() {
            cur = build(&mut ctx, cur, o);
        }
        cur
    };

    let mut opt = new_default_plan_cost_option();
    opt.with_cost_flag(c["flags"].as_u64().unwrap());
    let task_type = TaskType(c["task"].as_i64().unwrap() as isize);
    let cost = get_plan_cost(&ctx, root.as_ref(), task_type, &opt)?;
    let mut nodes = Vec::new();
    collect(root.as_ref(), &mut nodes);
    for name in FACTOR_VARS {
        *factor_var(ctx.vars, name) *= 2.0;
    }
    let again = get_plan_cost(&ctx, root.as_ref(), task_type, &opt)?;
    let mut out = serde_json::Map::new();
    out.insert("cost".into(), json!(f(cost)));
    out.insert("ranges".into(), json!(get_number_of_ranges(root.as_ref())));
    out.insert("nodes".into(), Value::Array(nodes));
    out.insert("again".into(), json!(f(again)));
    if let Some(rs) = reader_size {
        out.insert("reader_size".into(), json!(rs));
    }
    Ok(Value::Object(out))
}

fn factor(args: &[Value]) -> Value {
    let mut vars = SessionVars::new(Box::new(Acc));
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let fp = &args[2];
    let store = StoreType(fp["store"].as_u64().unwrap() as _);
    let tbl = Arc::new(table(
        fp["temp"].as_bool().unwrap(),
        false,
        false,
        &[TypeLonglong],
    ));
    let col = Arc::new(column(1, TypeLonglong));
    let mut ts = PhysicalTableScan {
        table: Arc::clone(&tbl),
        store_type: store,
        desc: fp["desc"].as_bool().unwrap(),
        ..Default::default()
    }
    .init(&ctx, 0);
    ts.set_schema(new_schema(vec![Arc::clone(&col)]));
    let ts: PhysicalPlanRef = Arc::new(ts);
    let idx = Arc::new(IndexInfo {
        id: 1,
        name: CIStr::new("idx"),
        columns: Some(vec![IndexColumn {
            offset: 0,
            length: UnspecifiedLength,
            ..Default::default()
        }]),
        ..Default::default()
    });
    let mut is = PhysicalIndexScan {
        table: Arc::clone(&tbl),
        index: idx,
        desc: fp["desc"].as_bool().unwrap(),
        data_source_schema: Some(new_schema(vec![Arc::clone(&col)])),
        ..Default::default()
    }
    .init(&ctx, 0);
    is.set_schema(new_schema(vec![Arc::clone(&col)]));
    let is: PhysicalPlanRef = Arc::new(is);
    let reader = |child: PhysicalPlanRef| -> PhysicalPlanRef {
        Arc::new(
            PhysicalTableReader {
                table_plan: Some(child),
                store_type: store,
                ..Default::default()
            }
            .init(&ctx, 0),
        )
    };
    let idx_reader = |child: PhysicalPlanRef| -> PhysicalPlanRef {
        Arc::new(
            PhysicalIndexReader {
                index_plan: Some(child),
                ..Default::default()
            }
            .init(&ctx, 0),
        )
    };
    let sel = |child: PhysicalPlanRef| -> PhysicalPlanRef {
        let mut s = PhysicalSelection {
            conditions: vec![Arc::new(Expr::Column((*col).clone()))],
            ..Default::default()
        }
        .init(
            &ctx,
            Some(Arc::new(StatsInfo {
                row_count: 1.0,
                ..Default::default()
            })),
            0,
            Vec::new(),
        );
        s.set_children(vec![child]);
        Arc::new(s)
    };
    let p: PhysicalPlanRef = match fp["kind"].as_str().unwrap() {
        "ts" => ts,
        "reader" => reader(ts),
        "sel_ts" => sel(ts),
        "sel_reader" => sel(reader(ts)),
        "is" => is,
        "ireader" => idx_reader(is),
        "sel_is" => sel(is),
        "sel_ireader" => sel(idx_reader(is)),
        "pg" => {
            let mut pg = PointGetPlan {
                tbl_info: Arc::clone(&tbl),
                schema: new_schema(vec![Arc::clone(&col)]),
                ..Default::default()
            };
            pg.base.plan = new_base_plan(&ctx, "Point_Get", 0);
            Arc::new(pg)
        }
        k => unreachable!("{k}"),
    };
    let tt = TaskType(args[1].as_i64().unwrap() as isize);
    let fac: &CostVer2Factor = match args[0].as_str().unwrap() {
        "cpu" => get_task_cpu_factor_ver2(p.as_ref(), tt),
        "mem" => get_task_mem_factor_ver2(p.as_ref(), tt),
        "scan" => get_task_scan_factor_ver2(p.as_ref(), store, tt),
        "net" => get_task_net_factor_ver2(p.as_ref(), tt),
        "request" => get_task_request_factor_ver2(p.as_ref(), tt),
        k => unreachable!("{k}"),
    };
    json!([
        fac.name,
        f(fac.value),
        String::from_utf8(fac.to_string()).unwrap()
    ])
}

fn formula(args: &[Value]) -> Value {
    let mut vars = SessionVars::new(Box::new(Acc));
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let (rows, x) = (fl(&args[1]), fl(&args[2]));
    let name = args[4].as_str().unwrap();
    let d = &*defaultVer2Factors;
    let fac = [
        &d.tidb_temp,
        &d.tikv_scan,
        &d.tidb_cpu,
        &d.tidb2_kv_net,
        &d.tidb_request,
        &d.tiflash_mem,
    ]
    .into_iter()
    .find(|f| f.name == name)
    .unwrap();
    let mut opt = new_default_plan_cost_option();
    if args[5].as_bool().unwrap() {
        opt.with_cost_flag(CostFlagTrace);
    }
    let col = column(1, TypeLonglong);
    let exprs: Vec<ExprRef> = args[3]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| expr(&ctx, k.as_str().unwrap(), &col))
        .collect();
    let by: Vec<ByItems> = exprs
        .iter()
        .map(|e| ByItems {
            expr: Arc::clone(e),
            desc: false,
        })
        .collect();
    let c = match args[0].as_str().unwrap() {
        "scan" => scan_cost_ver2(&opt, rows, x, fac),
        "net" => net_cost_ver2(&opt, rows, x, fac),
        "filter" => filter_cost_ver2(&opt, rows, &exprs, fac),
        "order" => order_cost_ver2(&opt, rows, x, &by, fac),
        "double_read" => double_read_cost_ver2(&opt, rows, fac),
        k => unreachable!("{k}"),
    };
    Value::Array(cost_out(&c))
}

fn replay(op: &str, args: &[Value]) -> Result<Value, Error> {
    match op {
        "plan" => plan(&args[0]),
        "factor" => Ok(factor(args)),
        "formula" => Ok(formula(args)),
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_costver2() {
    let mut s = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"))
        .read_to_string(&mut s)
        .unwrap();
    let mut lines = s.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "costver2");
    let mut by_op: BTreeMap<String, usize> = Default::default();
    let mut diffs = Vec::new();
    for line in lines {
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let args = rec["args"].as_array().unwrap();
        assert!(rec["err"].is_null(), "a Go error or panic record: {line}");
        let got = match catch_unwind(AssertUnwindSafe(|| replay(op, args))) {
            Ok(Ok(g)) => g,
            Ok(Err(e)) => {
                diffs.push(format!("{} {op}: error {e}", rec["src"]));
                continue;
            }
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
        "costver2: equal to Go {by_op:?}, {} differences",
        diffs.len()
    );
    for d in diffs.iter().take(10) {
        eprintln!("{d}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
}

/// The cost model v1 (`tidb_cost_model_version=1`, exception B5) is a boundary error, not
/// the base implementation's sum of the children; nothing is memoized.
#[test]
fn cost_model_v1_is_a_boundary() {
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.cost_model_version = 1;
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let mut ts = PhysicalTableScan {
        table: Arc::new(table(false, false, false, &[TypeLonglong])),
        ..Default::default()
    }
    .init(&ctx, 0);
    ts.set_stats(Some(Arc::new(StatsInfo {
        row_count: 10.0,
        ..Default::default()
    })));
    let opt = new_default_plan_cost_option();
    let err = get_plan_cost(&ctx, &ts, tidb_planner::property::RootTaskType, &opt).unwrap_err();
    assert_eq!(tidb_base::blocked::blocked_by(&err), Some("EXC.B"));
    assert!(!ts.base_physical().cost.lock().plan_cost_init);
}

/// `getCardinality`: a non-positive estimate is 1 under the cost model v2; with
/// `CostFlagUseTrueCardinality` the actual rows of the runtime stats are used.
#[test]
fn cardinality_estimated_and_true() {
    use tidb_planner::core::plan_cost_ver1::get_cardinality;
    use tidb_planner::util::costusage::CostFlagUseTrueCardinality;
    let mut vars = SessionVars::new(Box::new(Acc));
    let coll = tidb_execdetails::execdetails::RuntimeStatsColl::new(None);
    vars.stmt_ctx_mut().runtime_stats_coll = Some(Arc::clone(&coll));
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let mut ts = PhysicalTableScan::default().init(&ctx, 0);
    ts.set_stats(Some(Arc::new(StatsInfo {
        row_count: 0.0,
        ..Default::default()
    })));
    assert_eq!(get_cardinality(&ctx, &ts, 0), 1.0);
    // No runtime stats for the operator: 0 actual rows.
    assert_eq!(get_cardinality(&ctx, &ts, CostFlagUseTrueCardinality), 0.0);
    coll.get_basic_runtime_stats(i64::from(ts.id()))
        .set_row_num(42);
    assert_eq!(get_cardinality(&ctx, &ts, CostFlagUseTrueCardinality), 42.0);
}

/// The cost memo is written through `&self` on shared nodes: costing two candidate parents
/// over one shared reader / scan subtree (the `taskMap` case, VS1-26 spike #287) copies no
/// node (`clone_node` is never called) and fills the memo on the shared nodes in place.
#[test]
fn costing_shared_children_copies_no_node() {
    let mut vars = SessionVars::new(Box::new(Acc));
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(Vec::new()));
    let st = |rows: f64| {
        Some(Arc::new(StatsInfo {
            row_count: rows,
            ..Default::default()
        }))
    };
    let col = Arc::new(column(1, TypeLonglong));
    let mut ts = PhysicalTableScan {
        table: Arc::new(table(false, true, false, &[TypeLonglong])),
        tbl_cols: vec![Arc::clone(&col)],
        tbl_col_hists: Some(Arc::new(HistColl::default())),
        ranges: full_int_range(false),
        ..Default::default()
    }
    .init(&ctx, 0);
    ts.set_schema(new_schema(vec![Arc::clone(&col)]));
    ts.set_stats(st(100.0));
    let mut sel = PhysicalSelection {
        conditions: vec![expr(&ctx, "f", &col)],
        ..Default::default()
    }
    .init(&ctx, st(10.0), 0, Vec::new());
    sel.set_children(vec![Arc::new(ts)]);
    let mut reader = PhysicalTableReader {
        table_plan: Some(Arc::new(sel)),
        store_type: StoreType(0),
        ..Default::default()
    }
    .init(&ctx, 0);
    reader.set_stats(st(10.0));
    let shared: PhysicalPlanRef = Arc::new(reader);
    let by = vec![ByItems {
        expr: expr(&ctx, "c", &col),
        desc: false,
    }];
    let mut sort = PhysicalSort {
        by_items: by.clone(),
        ..Default::default()
    }
    .init(&ctx, st(10.0), 0, Vec::new());
    sort.set_children(vec![Arc::clone(&shared)]);
    let mut topn = PhysicalTopN {
        by_items: by,
        count: 5,
        ..Default::default()
    }
    .init(&ctx, st(5.0), 0, Vec::new());
    topn.set_children(vec![Arc::clone(&shared)]);
    let (sort, topn): (PhysicalPlanRef, PhysicalPlanRef) = (Arc::new(sort), Arc::new(topn));

    let opt = new_default_plan_cost_option();
    let rt = tidb_planner::property::RootTaskType;
    let before = tidb_planner::physicalop::clone_node_calls();
    let a = get_plan_cost(&ctx, sort.as_ref(), rt, &opt).unwrap();
    let b = get_plan_cost(&ctx, topn.as_ref(), rt, &opt).unwrap();
    // CostFlagRecalculate rewrites the memo, still in place.
    let mut recalc = new_default_plan_cost_option();
    recalc.with_cost_flag(tidb_planner::util::costusage::CostFlagRecalculate);
    get_plan_cost(&ctx, sort.as_ref(), rt, &recalc).unwrap();
    assert_eq!(tidb_planner::physicalop::clone_node_calls(), before);
    assert!(a > 0.0 && b > 0.0);
    assert!(Arc::ptr_eq(&sort.children()[0], &shared));
    assert!(Arc::ptr_eq(&topn.children()[0], &shared));
    assert_eq!(Arc::strong_count(&shared), 3);
    let r = shared
        .as_any()
        .downcast_ref::<PhysicalTableReader>()
        .unwrap();
    assert!(r.base_physical().cost.lock().plan_cost_init);
    let table_plan = r.table_plan.as_ref().unwrap();
    assert!(table_plan.base_physical().cost.lock().plan_cost_init);
    assert!(
        table_plan.children()[0]
            .base_physical()
            .cost
            .lock()
            .plan_cost_init
    );
    // The counter does see a copy-on-write copy of a shared node.
    let mut other = Arc::clone(&shared);
    tidb_planner::make_mut_plan(&mut other);
    assert_eq!(tidb_planner::physicalop::clone_node_calls(), before + 1);
}
