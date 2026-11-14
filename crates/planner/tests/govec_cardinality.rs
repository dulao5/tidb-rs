//! Replays the govec `cardinality` vectors (D10.6 / D11.1 part 1, VS1-29): the pseudo
//! estimation and the read part of the statistics structures, against Go TiDB v8.5.8
//! (tracker `govec/domains/cardinality.go`, which documents the record format).
//!
//! `selectivity` records whose conditions need ranges (D10.4, VS1-25), the IN builtin (D08.9)
//! or the D10.6 / D10.7 boundaries return `blocked-by:<block>` in tidb-rs: they are counted
//! per block and must not disagree otherwise (spec D10.6 §6).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_base::gostd::strconv::{format_float, parse_float};
use tidb_base::kv::StoreType;
use tidb_base::mysql::*;
use tidb_expression::scalar_function::new_function_internal;
use tidb_expression::{
    Column as ExprColumn, Constant, CorrelatedColumn, Expr, ExprRef, new_schema,
};
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::{FieldType, UnspecifiedLength};
use tidb_planner::PlanCtx;
use tidb_planner::cardinality as card;
use tidb_planner::property::{GroupNDV, StatsInfo};
use tidb_planner::util::path::AccessPath;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::ranger::types::Range;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};
use tidb_statistics::cmsketch::{TopN, TopNMeta, top_n_num, top_n_total_count};
use tidb_statistics::histogram::{
    new_histogram, new_stats_all_evicted_status, new_stats_full_load_status,
};
use tidb_statistics::table::{
    CacheItemMemoryUsage, ExtendedStatsItem, col_and_idx_existence_map_is_equal,
    new_col_and_index_existence_map_without_size, new_extended_stats_coll, new_hist_coll,
    new_hist_coll_with_cols_and_idxs,
};
use tidb_statistics::{
    Column, HistColl, Index, Table, column_stats_is_invalid, index_stats_is_invalid, pseudo_table,
};
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::*;
use tidb_types::field_type::new_field_type;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::time::{parse_duration, parse_time};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/cardinality.jsonl.gz",
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

fn session() -> SessionVars {
    SessionVars::new(Box::new(Acc))
}

// ---------------------------------------------------------------- inputs

fn i(v: &Value) -> i64 {
    v.as_i64()
        .unwrap_or_else(|| v.as_u64().map(|u| u as i64).unwrap_or(0))
}
fn b(v: &Value) -> bool {
    v.as_bool().unwrap()
}
fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}
fn fl(v: &Value) -> f64 {
    parse_float(v.as_str().expect("float string")).expect("float")
}
fn fo(f: f64) -> Value {
    json!(format_float(f, b'g', -1, 64))
}
fn arr(v: &Value) -> &Vec<Value> {
    v.as_array().unwrap()
}
fn lossy(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

fn datum(v: &Value) -> Datum {
    match v["k"].as_str().unwrap() {
        "null" => Datum::default(),
        "i" => new_int_datum(i(&v["v"])),
        "u" => new_uint_datum(v["v"].as_u64().unwrap()),
        "f" => new_float64_datum(fl(&v["v"])),
        "s" => {
            let c = v["c"]
                .as_str()
                .filter(|c| !c.is_empty())
                .unwrap_or("utf8mb4_bin");
            new_collation_string_datum(s(&v["v"]).into_bytes(), c.to_string())
        }
        "b" => new_bytes_datum(hex_decode(&s(&v["v"]))),
        "dec" => {
            let mut d = MyDecimal::default();
            d.from_string(s(&v["v"]).as_bytes()).unwrap();
            new_decimal_datum(d)
        }
        "dt" => {
            let (t, err) = parse_time(
                &DefaultStmtNoWarningContext,
                s(&v["v"]).as_bytes(),
                TypeDatetime,
                0,
            );
            err.unwrap();
            new_time_datum(t)
        }
        "dur" => {
            let (d, _, err) =
                parse_duration(&DefaultStmtNoWarningContext, s(&v["v"]).as_bytes(), 0);
            err.unwrap();
            new_duration_datum(d)
        }
        "min" => min_not_null_datum(),
        "max" => max_value_datum(),
        k => panic!("bad datum {k}"),
    }
}

fn datums(v: &Value) -> Vec<Datum> {
    arr(v).iter().map(datum).collect()
}

fn hex_decode(h: &str) -> Vec<u8> {
    (0..h.len())
        .step_by(2)
        .map(|k| u8::from_str_radix(&h[k..k + 2], 16).unwrap())
        .collect()
}

fn ranges(v: &Value) -> Vec<Range> {
    arr(v)
        .iter()
        .map(|r| Range {
            low_val: datums(&r[0]),
            high_val: datums(&r[1]),
            low_exclude: b(&r[2]),
            high_exclude: b(&r[3]),
            collators: arr(&r[4])
                .iter()
                .map(|c| collate::get_collator(&s(c)))
                .collect(),
        })
        .collect()
}

fn table(v: &Value) -> TableInfo {
    let mut t = TableInfo::default();
    t.unmarshal_json(s(v).as_bytes()).unwrap();
    t
}

fn cols(t: &TableInfo, v: &Value) -> Vec<Arc<ExprColumn>> {
    arr(v)
        .iter()
        .enumerate()
        .map(|(k, p)| {
            let id = i(&p[0]);
            let mut c = ExprColumn {
                id,
                unique_id: i(&p[1]),
                index: k as isize,
                ret_type: Arc::new(new_field_type(TypeLonglong)),
                ..ExprColumn::default()
            };
            for ci in t.columns.iter().flatten() {
                if ci.id == id {
                    c.ret_type = Arc::new(ci.field_type.clone());
                    c.is_hidden = ci.hidden;
                }
            }
            Arc::new(c)
        })
        .collect()
}

fn field_type(tp: &str) -> FieldType {
    match tp {
        "int" => new_field_type(TypeLonglong),
        "uint" => {
            let mut ft = new_field_type(TypeLonglong);
            ft.add_flag(UnsignedFlag);
            ft
        }
        "double" => new_field_type(TypeDouble),
        "float" => new_field_type(TypeFloat),
        "varchar" => {
            let mut ft = new_field_type(TypeVarchar);
            ft.set_charset("utf8mb4");
            ft.set_collate("utf8mb4_bin");
            ft.set_flen(32);
            ft
        }
        "blob" => new_field_type(TypeBlob),
        "decimal" => {
            let mut ft = new_field_type(TypeNewDecimal);
            ft.set_flen(10);
            ft.set_decimal(2);
            ft
        }
        "datetime" => new_field_type(TypeDatetime),
        "duration" => new_field_type(TypeDuration),
        "tiny" => new_field_type(TypeTiny),
        "enum" => {
            let mut ft = new_field_type(TypeEnum);
            ft.set_elems(Some(vec![b"a".to_vec(), b"b".to_vec()]));
            ft
        }
        "json" => new_field_type(TypeJSON),
        t => panic!("bad type {t}"),
    }
}

fn expr(ctx: &PlanCtx<'_>, cs: &[Arc<ExprColumn>], v: &Value) -> Option<ExprRef> {
    let a = arr(v);
    match a[0].as_str().unwrap() {
        "col" => Some(Expr::Column(ExprColumn::clone(&cs[i(&a[1]) as usize])).into_ref()),
        "cor" => Some(
            Expr::CorrelatedColumn(CorrelatedColumn::new(ExprColumn::clone(
                &cs[i(&a[1]) as usize],
            )))
            .into_ref(),
        ),
        "const" => {
            let d = datum(&a[1]);
            let mut ft = new_field_type(TypeLonglong);
            if d.kind() == KindString {
                ft = new_field_type(TypeVarchar);
                ft.set_charset("utf8mb4");
                ft.set_collate(d.collation());
            } else if d.is_null() {
                ft = new_field_type(TypeNull);
            }
            Some(Expr::Constant(Constant::new(d, ft)).into_ref())
        }
        "f" => {
            let mut args = Vec::new();
            for x in &a[2..] {
                args.push(expr(ctx, cs, x)?);
            }
            new_function_internal(
                &*ctx.expr_ctx,
                &s(&a[1]),
                new_field_type(TypeLonglong),
                args,
            )
        }
        k => panic!("bad expr {k}"),
    }
}

fn top_n(v: &Value) -> Option<Arc<TopN>> {
    let items = v.as_array()?;
    Some(Arc::new(TopN::new(
        items
            .iter()
            .map(|it| TopNMeta {
                encoded: hex_decode(&s(&it[0])),
                count: it[1].as_u64().unwrap(),
            })
            .collect(),
    )))
}

// ---------------------------------------------------------------- outputs

fn err_out(e: Error) -> Value {
    json!({"err": lossy(e.error())})
}

fn hist_out(hg: &tidb_statistics::Histogram) -> Value {
    json!([
        hg.id,
        hg.ndv,
        hg.null_count,
        hg.tp.get_type(),
        hg.tp.get_flag(),
        hg.tp.get_collate(),
        hg.len(),
        fo(hg.total_row_count()),
        fo(hg.not_null_count()),
        hg.tot_col_size
    ])
}

fn col_out(key: i64, c: &Column) -> Value {
    json!([
        key,
        hist_out(&c.histogram),
        c.is_handle,
        c.physical_id,
        c.info.as_ref().map(|i| i.id),
        c.is_stats_initialized(),
        c.is_full_load(),
        c.stats_ver
    ])
}

fn idx_out(key: i64, idx: &Index) -> Value {
    json!([
        key,
        hist_out(&idx.histogram),
        idx.physical_id,
        idx.info.as_ref().map(|i| i.id),
        idx.is_full_load(),
        idx.stats_ver
    ])
}

fn coll_out(coll: &HistColl) -> Value {
    let mut cs = vec![];
    coll.for_each_column_immutable(|k, c| {
        cs.push(col_out(k, c));
        false
    });
    let mut is = vec![];
    coll.for_each_index_immutable(|k, idx| {
        is.push(idx_out(k, idx));
        false
    });
    let map_out = |m: &BTreeMap<i64, Vec<i64>>| -> Value {
        Value::Array(m.iter().map(|(k, v)| json!([k, v])).collect())
    };
    let u2c: BTreeMap<i64, Vec<i64>> = coll
        .unique_id2_col_info_id
        .iter()
        .map(|(k, v)| (*k, vec![*v]))
        .collect();
    json!({
        "pid": coll.physical_id, "hpid": coll.have_physical_id, "rc": coll.realtime_count,
        "mc": coll.modify_count, "pseudo": coll.pseudo, "cntl": coll.can_not_trigger_load,
        "ver": coll.stats_ver, "colnum": coll.col_num(), "idxnum": coll.idx_num(), "cols": cs,
        "idxs": is, "i2c": map_out(&coll.idx2_col_unique_ids),
        "c2i": map_out(&coll.col_unique_id2_idx_ids), "u2c": map_out(&u2c),
        "mv": coll.mv_idx2_columns.len(),
    })
}

/// The outcome of a record: a value to compare, or a boundary of the port.
enum Got {
    Value(Value),
    Blocked(String),
}

fn res(r: Result<f64, Error>) -> Got {
    match r {
        Ok(f) => Got::Value(fo(f)),
        Err(e) => match blocked_by(&e) {
            Some(blk) => Got::Blocked(blk.to_string()),
            None => Got::Value(err_out(e)),
        },
    }
}

// ---------------------------------------------------------------- exec

fn exec_op(op: &str, p: &[Value]) -> Got {
    collate::set_new_collation_enabled_for_test(true);
    let mut vars = session();
    let ctx = PlanCtx::new(&mut vars, es::new_expr_context(vec![]));
    match op {
        "pseudo_table" => Got::Value(pseudo_table_op(&ctx, p)),
        "hist_coll" => {
            let t = table(&p[0]);
            let tbl = pseudo_table(&t, false, b(&p[1]));
            let cs = cols(&t, &p[2]);
            let generated = tbl
                .generate_hist_coll_from_column_info(&t, &cs)
                .expect("no MV index");
            let id2 = tbl.id2_unique_id(&cs);
            Got::Value(json!([coll_out(&generated), coll_out(&id2)]))
        }
        "row_count" => {
            let t = table(&p[0]);
            let tbl = pseudo_table(&t, b(&p[2]), b(&p[1]));
            let cs = cols(&t, &p[3]);
            let coll = tbl.generate_hist_coll_from_column_info(&t, &cs).unwrap();
            let id = i(&p[5]);
            let rs = ranges(&p[6]);
            res(match p[4].as_str().unwrap() {
                "col" => card::get_row_count_by_column_ranges(&ctx, &coll, id, &rs),
                "int" => card::get_row_count_by_int_column_ranges(&ctx, &coll, id, &rs),
                "idx" => card::get_row_count_by_index_ranges(&ctx, &coll, id, &rs, &[]),
                k => panic!("bad kind {k}"),
            })
        }
        "col_est" => {
            let t = table(&p[0]);
            let tbl = pseudo_table(&t, false, b(&p[1]));
            let d = datum(&p[4]);
            let id = i(&p[3]);
            if p[2] == "gt" {
                return Got::Value(fo(card::column_greater_row_count(&ctx, &tbl, &d, id)));
            }
            res(card::column_equal_row_count(&ctx, &tbl, &d, id))
        }
        "selectivity" => {
            let t = table(&p[0]);
            let tbl = pseudo_table(&t, false, true);
            let cs = cols(&t, &p[1]);
            let coll = tbl.generate_hist_coll_from_column_info(&t, &cs).unwrap();
            drop(ctx);
            vars.default_str_match_selectivity = fl(&p[3]);
            let ctx = PlanCtx::new(&mut vars, es::new_expr_context(vec![]));
            let mut exprs = Vec::new();
            for e in arr(&p[2]) {
                match expr(&ctx, &cs, e) {
                    Some(x) => exprs.push(x),
                    // The IN builtin is D08.9 (not ported): Go builds it, tidb-rs cannot yet.
                    None if e.to_string().contains(r#""f","in""#) => {
                        return Got::Blocked("D08.9".to_string());
                    }
                    None => return Got::Value(json!("nilexpr")),
                }
            }
            match card::selectivity(&ctx, &coll, &exprs, &[]) {
                Ok((sel, nodes)) => Got::Value(json!([fo(sel), nodes.len()])),
                Err(e) => match blocked_by(&e) {
                    Some(blk) => Got::Blocked(blk.to_string()),
                    None => Got::Value(err_out(e)),
                },
            }
        }
        "row_size" => {
            drop(ctx);
            vars.enable_chunk_rpc = b(&p[4]);
            let ctx = PlanCtx::new(&mut vars, es::new_expr_context(vec![]));
            Got::Value(row_size_op(&ctx, p))
        }
        "avg_col" => {
            let mut c = Column::with_histogram(new_histogram(
                1,
                0,
                i(&p[3]),
                0,
                Arc::new(field_type(&s(&p[0]))),
                0,
                i(&p[2]),
            ));
            c.is_handle = b(&p[1]);
            let n = i(&p[4]);
            Got::Value(json!([
                fo(card::avg_col_size(&c, n, true)),
                fo(card::avg_col_size(&c, n, false)),
                fo(card::avg_col_size_chunk_format(&c, n)),
                fo(card::avg_col_size_data_in_disk_by_rows(&c, n))
            ]))
        }
        "hist" => Got::Value(hist_op(p)),
        "col_stats" => Got::Value(col_stats_op(&ctx, p)),
        "existence" => Got::Value(existence_op(p)),
        "table_ops" => Got::Value(table_ops_op(p)),
        "ndv" => Got::Value(ndv_op(p)),
        "mv_sel" => {
            let coll = new_hist_coll(1, true, i(&p[0]), 0, 0, 0);
            let paths: Vec<AccessPath> = arr(&p[1])
                .iter()
                .map(|c| AccessPath {
                    is_int_handle_path: true,
                    count_after_access: fl(c),
                    ..Default::default()
                })
                .collect();
            Got::Value(fo(card::calc_total_selectivity_for_mv_idx_path(
                &coll,
                &paths,
                b(&p[2]),
            )))
        }
        "topn" => {
            let t = top_n(&p[0]);
            let c = t.as_ref().map(|t| Arc::new(t.copy()));
            Got::Value(json!([
                top_n_total_count(t.as_deref()),
                top_n_num(t.as_deref()),
                top_n_total_count(c.as_deref()),
                top_n_num(c.as_deref()),
                t.as_ref().map_or(0, |t| t.memory_usage()),
                c.as_ref().map_or(0, |c| c.memory_usage()),
                // Go's Equal: both empty, or the same items.
                (top_n_total_count(t.as_deref()) == 0 && top_n_total_count(c.as_deref()) == 0)
                    || t == c
            ]))
        }
        o => panic!("unknown op {o}"),
    }
}

fn pseudo_table_op(ctx: &PlanCtx<'_>, p: &[Value]) -> Value {
    let _ = ctx;
    let t = table(&p[0]);
    let tbl = pseudo_table(&t, b(&p[1]), b(&p[2]));
    let (h, ok) = Table::get_stats_healthy(Some(&tbl));
    let mem = tbl.memory_usage();
    let mut ndv = vec![];
    let mut by_name = vec![];
    let mut start_with = vec![];
    for c in t.columns.iter().flatten() {
        ndv.push(fo(card::estimate_column_ndv(&tbl, c.id)));
        by_name.push(json!(tbl.column_by_name(&c.name.l).map(|c| c.histogram.id)));
        let mut n = 0;
        tbl.for_each_index_immutable(|_, idx| {
            if idx.info.as_ref().unwrap().columns.as_ref().unwrap()[0]
                .name
                .l
                == c.name.l
            {
                n += 1;
            }
            false
        });
        if n <= 1 {
            start_with.push(json!(
                tbl.index_start_with_column(&c.name.l)
                    .map(|i| i.histogram.id)
            ));
        } else {
            start_with.push(json!("many"));
        }
    }
    ndv.push(fo(card::estimate_column_ndv(&tbl, 999)));
    let col_ids: Vec<i64> = tbl
        .stable_order_col_slice()
        .iter()
        .map(|c| c.histogram.id)
        .collect();
    let idx_ids: Vec<i64> = tbl
        .stable_order_idx_slice()
        .iter()
        .map(|i| i.histogram.id)
        .collect();
    let scaled: Vec<Value> = idx_ids
        .iter()
        .map(|id| {
            let (rc, mc) =
                tbl.get_scaled_realtime_and_modify_cnt(tbl.get_idx(*id).map(Arc::as_ref));
            json!([rc, mc])
        })
        .collect();
    let mut items = vec![];
    for id in &col_ids {
        items.push(tbl.get_col(*id).unwrap().item_id());
    }
    for id in &idx_ids {
        items.push(tbl.get_idx(*id).unwrap().item_id());
    }
    let exist = tbl.col_and_idx_existence_map.as_ref().unwrap();
    let mut ex = vec![];
    for c in t.columns.iter().flatten() {
        ex.push(json!([
            exist.has(c.id, false),
            exist.has_analyzed(c.id, false)
        ]));
    }
    for idx in t.indices.iter().flatten() {
        ex.push(json!([
            exist.has(idx.id, true),
            exist.has_analyzed(idx.id, true)
        ]));
    }
    json!({
        "coll": coll_out(&tbl.hist_coll), "str": tbl.to_string(), "healthy": [h, ok],
        "init": tbl.is_initialized(), "outdated": tbl.is_outdated(),
        "eligible": Table::is_eligible_for_analysis(Some(&tbl)),
        "meet": Table::meet_auto_analyze_min_cnt(Some(&tbl)), "analyzed": tbl.is_analyzed(),
        "arc": fo(tbl.get_analyze_row_count()),
        "mem": [mem.total_mem_usage, mem.total_tracking_mem_usage(), mem.total_col_tracking_mem_usage(),
            mem.total_idx_tracking_mem_usage(), mem.columns_mem_usage.len(), mem.indices_mem_usage.len()],
        "ndv": ndv, "byname": by_name, "startwith": start_with,
        "pavg": fo(card::pseudo_avg_count_per_value(&tbl)),
        "colids": col_ids, "idxids": idx_ids,
        "slices": [tbl.get_col_slice().len(), tbl.get_idx_slice().len()],
        "scaled": scaled, "items": items, "exist": ex, "exist_empty": exist.is_empty(),
        "exist_cols": exist.col_num(),
    })
}

fn row_size_op(ctx: &PlanCtx<'_>, p: &[Value]) -> Value {
    let mut cols_map = BTreeMap::new();
    for sc in arr(&p[2]) {
        let mut c = Column::with_histogram(new_histogram(
            i(&sc[0]),
            0,
            i(&sc[4]),
            0,
            Arc::new(field_type(&s(&sc[1]))),
            0,
            i(&sc[3]),
        ));
        c.is_handle = b(&sc[2]);
        cols_map.insert(i(&sc[0]), Arc::new(c));
    }
    let mut coll =
        new_hist_coll_with_cols_and_idxs(1, true, i(&p[1]), 0, cols_map, BTreeMap::new());
    coll.pseudo = b(&p[0]);
    let expr_cols: Vec<Arc<ExprColumn>> = arr(&p[3])
        .iter()
        .map(|q| {
            Arc::new(ExprColumn {
                unique_id: i(&q[0]),
                ret_type: Arc::new(field_type(&s(&q[1]))),
                ..ExprColumn::default()
            })
        })
        .collect();
    let (is_key, is_scan) = (b(&p[5]), b(&p[6]));
    let st = StoreType(i(&p[7]) as u8);
    json!([
        fo(card::get_avg_row_size(
            ctx, &coll, &expr_cols, is_key, is_scan
        )),
        fo(card::get_index_avg_row_size(
            ctx,
            &coll,
            &expr_cols,
            b(&p[9])
        )),
        fo(card::get_table_avg_row_size(
            ctx,
            &coll,
            &expr_cols,
            st,
            b(&p[8])
        )),
        fo(card::get_avg_row_size_data_in_disk_by_rows(
            &coll, &expr_cols
        ))
    ])
}

fn hist_op(p: &[Value]) -> Value {
    let tp = Arc::new(field_type(&s(&p[0])));
    let buckets = arr(&p[1]);
    let mut hg = new_histogram(1, i(&p[2]), i(&p[3]), 0, tp, buckets.len(), 0);
    for bk in buckets {
        hg.append_bucket_with_ndv(
            &datum(&bk[0]),
            &datum(&bk[1]),
            i(&bk[2]),
            i(&bk[3]),
            i(&bk[4]),
        );
    }
    let mut c = Column::with_histogram(hg);
    c.stats_ver = i(&p[4]);
    c.top_n = top_n(&p[5]);
    c.stats_loaded_status = new_stats_full_load_status();
    let mut cols_map = BTreeMap::new();
    cols_map.insert(1, Arc::new(c));
    let mut coll = new_hist_coll_with_cols_and_idxs(1, true, 0, 0, cols_map, BTreeMap::new());
    coll.calc_pre_scalar();
    let c = Arc::clone(coll.get_col(1).unwrap());
    let hg = &c.histogram;
    let vals = datums(&p[6]);
    let (rc, mc) = (i(&p[7]), i(&p[8]));
    let mut per = vec![];
    for v in &vals {
        let less = hg.less_row_count(v);
        let (eqc, eqm) = hg.equal_row_count(v, false);
        let (eqc_n, eqm_n) = hg.equal_row_count(v, true);
        let (ex, bi, inb, m) = hg.locate_bucket(v);
        let under =
            card::is_last_bucket_end_value_underrepresented(hg, v, eqc_n, hg.ndv as f64, rc, mc);
        per.push(json!([
            fo(less),
            fo(eqc),
            eqm,
            fo(eqc_n),
            eqm_n,
            fo(hg.greater_row_count(v)),
            [ex, bi, inb, m],
            under
        ]));
    }
    let mut between = vec![];
    for k in 0..vals.len().saturating_sub(1) {
        between.push(fo(hg.between_row_count(&vals[k], &vals[k + 1])));
    }
    let mut bounds = vec![];
    for k in 0..hg.len() {
        bounds.push(json!([
            lossy(hg.get_lower(k).to_string().unwrap()),
            lossy(hg.get_upper(k).to_string().unwrap()),
            hg.bucket_count(k)
        ]));
    }
    let cp = hg.copy();
    json!({
        "per": per, "between": between, "bounds": bounds, "len": hg.len(),
        "tot": fo(hg.total_row_count()), "nn": fo(hg.not_null_count()), "str": hg.to_string(0),
        "copy": cp.to_string(0) == hg.to_string(0), "ctot": fo(c.total_row_count()),
        "cnn": fo(c.not_null_count()), "inc": fo(c.get_increase_factor(rc)),
        "abs": fo(hg.abs_row_count_difference(rc)),
    })
}

fn col_stats_op(ctx: &PlanCtx<'_>, p: &[Value]) -> Value {
    let vars = &*ctx.vars;
    let is_index = b(&p[0]);
    let (stats_ver, ndv, null_count, count) = (i(&p[1]), i(&p[2]), i(&p[3]), i(&p[4]));
    let topn = top_n(&p[5]);
    let mut coll = new_hist_coll(7, true, i(&p[10]), 0, 0, 0);
    coll.pseudo = b(&p[7]);
    coll.can_not_trigger_load = b(&p[8]);
    let cid = i(&p[9]);
    let mut tp = new_field_type(TypeLonglong);
    let (mut lo, mut hi) = (new_int_datum(1), new_int_datum(100));
    if is_index {
        tp = new_field_type(TypeBlob);
        let utc = tidb_base::gostd::time::utc();
        let mut l = Vec::new();
        tidb_codec::codec::encode_key(&utc, &mut l, &[lo]).unwrap();
        let mut h = Vec::new();
        tidb_codec::codec::encode_key(&utc, &mut h, &[hi]).unwrap();
        lo = new_bytes_datum(l);
        hi = new_bytes_datum(h);
    }
    let mut hg = new_histogram(cid, ndv, null_count, 0, Arc::new(tp), 1, 0);
    if count > 0 {
        hg.append_bucket_with_ndv(&lo, &hi, count, 1, 0);
    }
    let st = match p[6].as_str().unwrap() {
        "full" => new_stats_full_load_status(),
        "evicted" => new_stats_all_evicted_status(),
        _ => Default::default(),
    };
    let st_out = json!([
        st.is_stats_initialized(),
        st.is_load_needed(),
        st.is_essential_stats_loaded(),
        st.is_all_evicted(),
        st.is_full_load(),
        st.status_to_string()
    ]);
    if is_index {
        let info = Arc::new(IndexInfo {
            id: cid,
            name: CIStr::new("i"),
            columns: Some(vec![IndexColumn {
                name: CIStr::new("a"),
                length: UnspecifiedLength,
                ..Default::default()
            }]),
            ..Default::default()
        });
        let mut idx = Index::with_histogram(hg);
        idx.stats_ver = stats_ver;
        idx.top_n = topn.clone();
        idx.info = Some(info);
        idx.stats_loaded_status = st;
        idx.physical_id = 7;
        let mem = idx.memory_usage();
        let mut out = vec![
            json!(index_stats_is_invalid(vars, Some(&idx), &coll, cid)),
            json!(index_stats_is_invalid(vars, None, &coll, cid)),
            fo(idx.total_row_count()),
            fo(idx.get_increase_factor(1000)),
            json!(Index::is_all_evicted(Some(&idx))),
            json!(Index::is_all_evicted(None)),
            json!(idx.get_evicted_status()),
            json!(idx.is_cms_exist()),
            json!(idx.is_evicted()),
            json!(idx.is_analyzed()),
            json!(idx.item_id()),
            json!(idx.to_string()),
            json!(idx.copy().to_string()),
            json!(idx.get_stats_ver()),
            json!(idx.get_histogram().id),
            json!(top_n_num(idx.get_top_n())),
            st_out,
            json!(mem.total_memory_usage()),
            json!(mem.tracking_mem_usage()),
        ];
        if topn.is_none() {
            out.push(json!(idx.query_bytes(&[1, 2]).unwrap()));
        } else {
            out.push(Value::Null);
        }
        idx.evict_all_stats();
        out.push(json!(Index::is_all_evicted(Some(&idx))));
        out.push(json!(idx.get_evicted_status()));
        out.push(json!(idx.histogram.len()));
        return Value::Array(out);
    }
    let info = Arc::new(ColumnInfo {
        id: cid,
        name: CIStr::new("a"),
        field_type: new_field_type(TypeLonglong),
        ..Default::default()
    });
    let mut c = Column::with_histogram(hg);
    c.stats_ver = stats_ver;
    c.top_n = topn;
    c.info = Some(Arc::clone(&info));
    c.stats_loaded_status = st;
    c.physical_id = 7;
    let empty = tidb_statistics::column::empty_column(7, true, &info);
    let mem = c.memory_usage();
    json!([
        column_stats_is_invalid(Some(&c), Some(vars), &coll, cid),
        column_stats_is_invalid(None, Some(vars), &coll, cid),
        fo(c.total_row_count()),
        fo(c.not_null_count()),
        fo(c.get_increase_factor(1000)),
        Column::is_all_evicted(Some(&c)),
        Column::is_all_evicted(None),
        c.get_evicted_status(),
        c.is_cms_exist(),
        c.is_analyzed(),
        c.stats_available(),
        c.item_id(),
        c.to_string(),
        c.copy().to_string(),
        c.get_stats_ver(),
        c.get_histogram().id,
        top_n_num(c.get_top_n()),
        st_out,
        mem.total_memory_usage(),
        mem.tracking_mem_usage(),
        empty.is_handle,
        empty.to_string()
    ])
}

fn existence_op(p: &[Value]) -> Value {
    let mut m = new_col_and_index_existence_map_without_size();
    let mut clone = None;
    let mut out = vec![];
    for st in arr(&p[0]) {
        let id = i(&st[1]);
        match st[0].as_str().unwrap() {
            "insc" => m.insert_col(id, b(&st[2])),
            "insi" => m.insert_index(id, b(&st[2])),
            "delc" => m.delete_col_not_found(id),
            "deli" => m.delete_idx_not_found(id),
            "clone" => clone = Some(m.clone_map()),
            k => panic!("bad step {k}"),
        }
        let eq = clone
            .as_ref()
            .is_some_and(|c| col_and_idx_existence_map_is_equal(&m, c));
        out.push(json!([
            m.has(id, false),
            m.has(id, true),
            m.has_analyzed(id, false),
            m.has_analyzed(id, true),
            m.is_empty(),
            m.col_num(),
            eq
        ]));
    }
    Value::Array(out)
}

fn table_ops_op(p: &[Value]) -> Value {
    let t = table(&p[0]);
    let mut tbl = pseudo_table(&t, false, true);
    let mut cp = tbl.copy();
    let sh = tbl.shallow_copy();
    let copy = json!([
        cp.to_string() == tbl.to_string(),
        coll_out(&cp.hist_coll),
        sh.to_string() == tbl.to_string(),
        coll_out(&sh.hist_coll),
        col_and_idx_existence_map_is_equal(
            cp.col_and_idx_existence_map.as_ref().unwrap(),
            tbl.col_and_idx_existence_map.as_ref().unwrap()
        )
    ]);
    let mut infos = vec![];
    for c in t.columns.iter().flatten() {
        for need in [false, true] {
            match tbl.get_stats_info(c.id, false, need) {
                Some((hg, cms, topn, fms)) => infos.push(json!([
                    true,
                    hg.id,
                    cms.is_some(),
                    topn.is_some(),
                    fms.is_some()
                ])),
                None => infos.push(json!([false, null, false, false, false])),
            }
        }
    }
    for idx in t.indices.iter().flatten() {
        match tbl.get_stats_info(idx.id, true, true) {
            Some((hg, ..)) => infos.push(json!([true, hg.id])),
            None => infos.push(json!([false, null])),
        }
    }
    if let Some(c) = t.columns.iter().flatten().next() {
        cp.del_col(c.id);
    }
    if let Some(idx) = t.indices.iter().flatten().next() {
        cp.del_idx(idx.id);
    }
    let z = Arc::new(ColumnInfo {
        id: 500,
        name: CIStr::new("z"),
        field_type: new_field_type(TypeLonglong),
        ..Default::default()
    });
    cp.set_col(
        500,
        Arc::new(tidb_statistics::column::empty_column(t.id, false, &z)),
    );
    let mut i600 = Index::with_histogram(new_histogram(
        600,
        0,
        0,
        0,
        Arc::new(new_field_type(TypeBlob)),
        0,
        0,
    ));
    i600.info = Some(Arc::new(IndexInfo {
        id: 600,
        ..Default::default()
    }));
    cp.set_idx(600, Arc::new(i600));
    let del = json!([
        coll_out(&cp.hist_coll),
        cp.col_and_idx_existence_map.as_ref().unwrap().col_num(),
        cp.col_and_idx_existence_map.as_ref().unwrap().is_empty()
    ]);
    let mut healthy = vec![];
    for mc in [0, 1, 3000, 9999, 10000, 20000] {
        let mut h = tbl.clone();
        h.pseudo = false;
        h.last_analyze_version = 1;
        h.modify_count = mc;
        let (hh, ok) = Table::get_stats_healthy(Some(&h));
        healthy.push(json!([
            hh,
            ok,
            h.is_outdated(),
            Table::is_eligible_for_analysis(Some(&h)),
            Table::meet_auto_analyze_min_cnt(Some(&h))
        ]));
    }
    let mut ext = new_extended_stats_coll();
    let item = Arc::new(ExtendedStatsItem {
        col_ids: vec![1, 2],
        scalar_vals: 0.5,
        ..Default::default()
    });
    ext.stats.insert("s1".to_string(), Arc::clone(&item));
    ext.last_update_version = 7;
    let mut with_ext = tbl.shallow_copy();
    with_ext.extended_stats = Some(Arc::new(ext));
    let ce = with_ext.copy();
    let ce_ext = ce.extended_stats.as_ref().unwrap();
    let ext_out = json!([
        ce_ext.stats.len(),
        ce_ext.last_update_version,
        Arc::ptr_eq(&ce_ext.stats["s1"], &item)
    ]);
    let hc = new_hist_coll(9, true, 30, 4, 2, 1);
    tbl.release_and_put_to_pool();
    json!({
        "copy": copy, "infos": infos, "del": del, "healthy": healthy, "ext": ext_out,
        "new": coll_out(&hc), "release": [tbl.col_num(), tbl.idx_num()],
    })
}

fn ndv_op(p: &[Value]) -> Value {
    let schema = new_schema(
        arr(&p[0])
            .iter()
            .map(|u| {
                Arc::new(ExprColumn {
                    unique_id: i(u),
                    ret_type: Arc::new(new_field_type(TypeLonglong)),
                    ..ExprColumn::default()
                })
            })
            .collect(),
    );
    let mut profile = StatsInfo {
        col_ndvs: HashMap::new(),
        ..Default::default()
    };
    for nd in arr(&p[1]) {
        profile.col_ndvs.insert(i(&nd[0]), fl(&nd[1]));
    }
    for g in arr(&p[2]) {
        profile.group_ndvs.push(GroupNDV {
            cols: arr(&g[0]).iter().map(i).collect(),
            ndv: fl(&g[1]),
        });
    }
    let q: Vec<i64> = arr(&p[3]).iter().map(i).collect();
    let qcols: Vec<Arc<ExprColumn>> = q
        .iter()
        .map(|u| {
            Arc::new(ExprColumn {
                unique_id: *u,
                ret_type: Arc::new(new_field_type(TypeLonglong)),
                ..ExprColumn::default()
            })
        })
        .collect();
    let (n1, l1) = card::estimate_cols_ndv_with_matched_len(&qcols, &schema, &profile);
    let (n2, l2) = card::estimate_cols_dnv_with_matched_len_from_unique_ids(&q, &schema, &profile);
    json!([fo(n1), l1, fo(n2), l2])
}

// ---------------------------------------------------------------- replay

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "cardinality");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut blocked: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut passed: BTreeMap<String, usize> = BTreeMap::new();
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap().to_string();
        let got = catch_unwind(AssertUnwindSafe(|| exec_op(&op, &args)));
        let go_panicked = rec["err"]["name"] == "panic";
        // A Go error is compared as {"err": text}.
        let want = if rec["err"].is_null() {
            rec["out"].clone()
        } else {
            json!({"err": rec["err"]["msg"]})
        };
        match got {
            Err(_) if go_panicked => {}
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                fails.push(format!("{line}\n    rust panicked: {msg:?}"));
            }
            Ok(Got::Blocked(blk)) => {
                *blocked.entry((op.clone(), blk)).or_default() += 1;
            }
            Ok(Got::Value(got)) if go_panicked => {
                fails.push(format!("{line}\n    rust returned {got}"))
            }
            Ok(Got::Value(got)) => {
                if got != want {
                    fails.push(format!("want {want} got {got}\n    {line}"));
                } else {
                    *passed.entry(op.clone()).or_default() += 1;
                }
            }
        }
    }
    std::panic::set_hook(prev);
    if let Ok(dump) = std::env::var("GOVEC_CARDINALITY_DUMP") {
        std::fs::write(dump, fails.join("\n")).unwrap();
    }
    eprintln!("{path}: {n} records, passed {passed:?}, blocked {blocked:?}");
    let shown: Vec<_> = fails
        .iter()
        .take(20)
        .map(|s| s.chars().take(3000).collect::<String>())
        .collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    // Only the selectivity records may stop at a boundary, and only at the expected ones.
    for (op, blk) in blocked.keys() {
        assert_eq!(op, "selectivity", "unexpected boundary {blk} in {op}");
        assert!(
            ["D08.9", "D10.4", "D10.6", "D10.7"].contains(&blk.as_str()),
            "unexpected boundary {blk}"
        );
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_cardinality() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_CARDINALITY_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}

/// Go `TestPseudoTable` (pkg/planner/cardinality/row_count_test.go:28): the unexported
/// `columnLessRowCount` / `columnBetweenRowCount` are not reachable from govec.
#[test]
fn pseudo_table_go_test() {
    let col_info = Arc::new(ColumnInfo {
        id: 1,
        field_type: new_field_type(TypeLonglong),
        state: tidb_model::StatePublic,
        ..Default::default()
    });
    let mut ti = TableInfo {
        columns: Some(vec![Arc::clone(&col_info)]),
        ..Default::default()
    };
    let tbl = pseudo_table(&ti, false, false);
    assert_eq!(tbl.col_num(), 0);
    assert!(tbl.realtime_count > 0);
    let mut vars = session();
    let sctx = PlanCtx::new(&mut vars, es::new_expr_context(vec![]));
    let count = card::column_less_row_count(&sctx, &tbl, &new_int_datum(100), col_info.id);
    assert_eq!(count as i64, 3333);
    let count =
        card::column_equal_row_count(&sctx, &tbl, &new_int_datum(1000), col_info.id).unwrap();
    assert_eq!(count as i64, 10);
    let count = card::column_between_row_count(
        &sctx,
        &tbl,
        &new_int_datum(1000),
        &new_int_datum(5000),
        col_info.id,
    )
    .unwrap();
    assert_eq!(count as i64, 250);
    ti.columns.as_mut().unwrap().push(Arc::new(ColumnInfo {
        id: 2,
        field_type: new_field_type(TypeLonglong),
        hidden: true,
        state: tidb_model::StatePublic,
        ..Default::default()
    }));
    let tbl = pseudo_table(&ti, false, false);
    // We added a hidden column. The pseudo table still only have zero column.
    assert_eq!(tbl.col_num(), 0);
}

/// N13 (spec D11.1 §3): the pseudo statistics share the `TableInfo`'s column and index
/// `Arc`s; a shallow copy shares the maps and a write through it copies first.
#[test]
fn statistics_share_metadata_and_copy_on_write() {
    let col = Arc::new(ColumnInfo {
        id: 1,
        name: CIStr::new("a"),
        field_type: new_field_type(TypeLonglong),
        state: tidb_model::StatePublic,
        ..Default::default()
    });
    let idx = Arc::new(IndexInfo {
        id: 1,
        name: CIStr::new("i"),
        state: tidb_model::StatePublic,
        columns: Some(vec![IndexColumn {
            name: CIStr::new("a"),
            length: UnspecifiedLength,
            ..Default::default()
        }]),
        ..Default::default()
    });
    let ti = TableInfo {
        id: 9,
        columns: Some(vec![Arc::clone(&col)]),
        indices: Some(vec![Arc::clone(&idx)]),
        ..Default::default()
    };
    let tbl = Arc::new(pseudo_table(&ti, false, true));
    assert!(Arc::ptr_eq(
        tbl.get_col(1).unwrap().info.as_ref().unwrap(),
        &col
    ));
    assert!(Arc::ptr_eq(
        tbl.get_idx(1).unwrap().info.as_ref().unwrap(),
        &idx
    ));
    // A statement's view: the shared snapshot, then a shallow copy it may change.
    let mut mine = tbl.shallow_copy();
    assert!(Arc::ptr_eq(
        mine.get_col(1).unwrap(),
        tbl.get_col(1).unwrap()
    ));
    mine.del_col(1);
    mine.realtime_count = 5;
    assert_eq!(mine.col_num(), 0);
    assert_eq!(tbl.col_num(), 1, "the shared table is not written");
    assert_eq!(tbl.realtime_count, 10000);
    assert!(
        tbl.col_and_idx_existence_map
            .as_ref()
            .unwrap()
            .has(1, false)
    );
    // Go `Copy` is a deep copy: nothing shared.
    let deep = tbl.copy();
    assert!(!Arc::ptr_eq(
        deep.get_col(1).unwrap(),
        tbl.get_col(1).unwrap()
    ));
    assert!(!Arc::ptr_eq(
        deep.get_col(1).unwrap().info.as_ref().unwrap(),
        &col
    ));
    // The query's HistColl shares the column statistics.
    let ecol = Arc::new(ExprColumn {
        id: 1,
        unique_id: 77,
        ret_type: Arc::new(new_field_type(TypeLonglong)),
        ..ExprColumn::default()
    });
    let coll = tbl
        .generate_hist_coll_from_column_info(&ti, &[ecol])
        .unwrap();
    assert!(Arc::ptr_eq(
        coll.get_col(77).unwrap(),
        tbl.get_col(1).unwrap()
    ));
    assert_eq!(coll.idx2_col_unique_ids[&1], vec![77]);
}
