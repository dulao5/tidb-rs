#![allow(non_upper_case_globals)]
//! Replays the govec `planutil` vectors (tidb-rs-tracker `govec/domains/planutil.go`): block
//! D10.10, the VS1-19 part — `pkg/planner/property` and `pkg/planner/util` — against Go TiDB
//! v8.5.8.
//!
//! `GOVEC_PLANUTIL_FILES` (comma-separated paths) replays extra artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::errors::RedactLogDisable;
use tidb_base::gostd::strconv::{format_float, parse_float};
use tidb_base::gostd::time::unix;
use tidb_base::kv::StoreType;
use tidb_base::mysql::{PriKeyFlag, TypeLonglong, TypeVarchar, UnsignedFlag};
use tidb_chunk::mutrow::mut_row_from_datums;
use tidb_expression::scalar_function::new_function_internal;
use tidb_expression::{Column, CorrelatedColumn, Expr, ExprRef, ast_funcs, new_schema};
use tidb_kv::Handle;
use tidb_model::{ColumnInfo, IndexColumn, IndexInfo, TableInfo};
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_planner::property::{
    self, CteProducerStatus, GroupNDV, MPPPartitionColumn, MPPPartitionType, PartialOrderInfo,
    PhysicalPropMatchResult, PhysicalProperty, SortItem, StatsInfo, TaskType,
};
use tidb_planner::util::byitem::{ByItems, stringify_by_items_with_ctx};
use tidb_planner::util::column::{
    index_info2_cols, index_info2_full_cols, index_info2_prefix_cols,
};
use tidb_planner::util::costusage::{
    CostFlagTrace, CostVer2, CostVer2Factor, div_cost_ver2, has_cost_flag, mul_cost_ver2,
    new_cost_ver2, new_zero_cost_ver2, sum_cost_ver2, trace_cost,
};
use tidb_planner::util::handle_cols::{
    HandleCols, get_common_handle_datum, new_common_handle_cols,
    new_common_handles_cols_without_cols_align, new_int_handle_cols,
};
use tidb_planner::util::misc::{
    QueryTimeRange, derive_limit_stats, encode_int_as_uint32, get_max_sort_prefix,
};
use tidb_planner::util::optimizetrace::new_default_plan_cost_option;
use tidb_planner::util::path::{AccessPath, Col2Len, compare_col2_len};
use tidb_sessionctx::exprctx::StaticConvertibleExprContext as _;
use tidb_sessionctx::exprstatic as es;
use tidb_sessionctx::ranger::types::Range;
use tidb_sessionctx::stmtctx::new_stmt_ctx;
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::{
    Datum, KindInt64, KindString, max_value_datum, min_not_null_datum, new_collation_string_datum,
    new_int_datum, new_uint_datum,
};
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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/planutil.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
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
fn gs(b: Vec<u8>) -> Value {
    json!(String::from_utf8(b).unwrap())
}

fn datum(v: &Value) -> Datum {
    match v["k"].as_str().unwrap() {
        "null" => Datum::default(),
        "i" => new_int_datum(i(&v["v"])),
        "u" => new_uint_datum(v["v"].as_u64().unwrap()),
        "s" => new_collation_string_datum(s(&v["v"]).into_bytes(), "utf8mb4_bin"),
        "min" => min_not_null_datum(),
        "max" => max_value_datum(),
        k => panic!("bad datum {k}"),
    }
}

fn datums(v: &Value) -> Vec<Datum> {
    arr(v).iter().map(datum).collect()
}

fn field_type(tp: &str, flen: isize) -> FieldType {
    match tp {
        "int" => {
            let mut ft = new_field_type(TypeLonglong);
            if flen > 0 {
                ft.set_flen(flen);
            }
            ft
        }
        "str" => {
            let mut ft = new_field_type(TypeVarchar);
            ft.set_charset("utf8mb4");
            ft.set_collate("utf8mb4_bin");
            ft.set_flen(flen);
            ft
        }
        _ => panic!("bad type {tp}"),
    }
}

fn col(v: &Value) -> Arc<Column> {
    let a = arr(v);
    Arc::new(Column {
        unique_id: i(&a[0]),
        id: i(&a[0]),
        ret_type: field_type(a[1].as_str().unwrap(), i(&a[2]) as isize),
        ..Default::default()
    })
}

fn int_col(id: i64) -> Column {
    Column {
        unique_id: id,
        id,
        ret_type: new_field_type(TypeLonglong),
        ..Default::default()
    }
}

fn ranges(v: &Value) -> Vec<Range> {
    arr(v)
        .iter()
        .map(|r| {
            let r = arr(r);
            let low = datums(&r[0]);
            Range {
                collators: collate::get_binary_collator_slice(low.len()),
                low_val: low,
                high_val: datums(&r[1]),
                low_exclude: b(&r[2]),
                high_exclude: b(&r[3]),
            }
        })
        .collect()
}

fn sort_items(v: &Value) -> Vec<SortItem> {
    arr(v)
        .iter()
        .map(|it| SortItem {
            col: Arc::new(int_col(i(&it[0]))),
            desc: b(&it[1]),
        })
        .collect()
}

fn sort_items_out(items: &[SortItem]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|it| json!([it.col.unique_id, it.desc]))
            .collect(),
    )
}

fn col2len(v: &Value) -> Col2Len {
    let mut m = Col2Len::new();
    for p in arr(v) {
        m.insert(i(&p[0]), i(&p[1]) as isize);
    }
    m
}

fn col2len_out(m: &Col2Len) -> Value {
    let mut keys: Vec<&i64> = m.keys().collect();
    keys.sort();
    Value::Array(keys.into_iter().map(|k| json!([k, m[k]])).collect())
}

fn ints_out(xs: &[isize]) -> Value {
    json!(xs)
}

fn schema_of(v: &Value) -> tidb_expression::Schema {
    new_schema(arr(v).iter().map(|id| Arc::new(int_col(i(id)))).collect())
}

// ---------------------------------------------------------------- ops

fn index_cols(p: &[Value]) -> Value {
    let col_infos: Vec<ColumnInfo> = arr(&p[0])
        .iter()
        .map(|ci| ColumnInfo {
            name: CIStr::new(ci[0].as_str().unwrap()),
            field_type: field_type("str", i(&ci[1]) as isize),
            ..Default::default()
        })
        .collect();
    let cols: Vec<Arc<Column>> = arr(&p[1]).iter().map(col).collect();
    let index = IndexInfo {
        columns: Some(
            arr(&p[2])
                .iter()
                .map(|ic| IndexColumn {
                    name: CIStr::new(ic[0].as_str().unwrap()),
                    length: i(&ic[1]) as isize,
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    };
    let c = |c: &Arc<Column>| json!([c.unique_id, c.is_prefix]);
    let oc = |o: &Option<Arc<Column>>| o.as_ref().map_or(Value::Null, c);
    let (pc, pl, fc, flens) = index_info2_cols(&col_infos, &cols, &index);
    let (pc2, pl2) = index_info2_prefix_cols(&col_infos, &cols, &index);
    let (fc2, fl2) = index_info2_full_cols(&col_infos, &cols, &index);
    json!({
        "pc": pc.iter().map(c).collect::<Vec<_>>(), "pl": ints_out(&pl),
        "fc": fc.iter().map(oc).collect::<Vec<_>>(), "fl": ints_out(&flens),
        "pc2": pc2.iter().map(c).collect::<Vec<_>>(), "pl2": ints_out(&pl2),
        "fc2": fc2.iter().map(oc).collect::<Vec<_>>(), "fl2": ints_out(&fl2),
    })
}

fn prop_in(v: &Value) -> PhysicalProperty {
    let items = sort_items(&v["si"]);
    let cols: Vec<Arc<Column>> = items.iter().map(|it| Arc::clone(&it.col)).collect();
    let desc = items.first().is_some_and(|it| it.desc);
    let mut p = property::new_physical_property(
        TaskType(i(&v["tt"]) as isize),
        &cols,
        desc,
        fl(&v["ec"]),
        b(&v["en"]),
    );
    for (k, it) in p.sort_items.iter_mut().enumerate() {
        it.desc = items[k].desc;
    }
    p.sort_items_for_partition = sort_items(&v["sp"]);
    p.reject_sort = b(&v["rs"]);
    p.cte_producer_status = CteProducerStatus(i(&v["cte"]) as isize);
    if !v["po"].is_null() {
        p.partial_order_info = Some(Arc::new(PartialOrderInfo {
            sort_items: sort_items(&v["po"]),
        }));
    }
    p.advisory_sort_items = sort_items(&v["ad"]);
    p.mpp_partition_tp = MPPPartitionType(i(&v["mt"]) as isize);
    for mc in arr(&v["mc"]) {
        p.mpp_partition_cols.push(Arc::new(MPPPartitionColumn {
            col: Arc::new(int_col(i(&mc[0]))),
            collate_id: i(&mc[1]) as i32,
        }));
    }
    p
}

fn mpp_cols(v: &Value) -> Vec<Arc<MPPPartitionColumn>> {
    arr(v)
        .iter()
        .map(|k| {
            Arc::new(MPPPartitionColumn {
                col: Arc::new(int_col(i(&k[0]))),
                collate_id: i(&k[1]) as i32,
            })
        })
        .collect()
}

fn hexs(b: &[u8]) -> Value {
    json!(b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

fn prop(p: &[Value]) -> Value {
    let prop = prop_in(&p[0]);
    let other = prop_in(&p[1]);
    let schema = schema_of(&p[2]);
    let keys = mpp_cols(&p[3]);
    let mem0 = prop.memory_usage();
    let (same, desc) = prop.all_same_order();
    let child: Vec<&str> = prop
        .get_all_possible_child_task_types()
        .iter()
        .map(|t| t.to_string())
        .collect();
    let mpp: Vec<Value> = prop
        .mpp_partition_cols
        .iter()
        .map(|c| {
            let eq: Vec<bool> = keys.iter().map(|k| c.equal(k)).collect();
            json!({"hash": hexs(&c.hash_code()), "mem": c.memory_usage(),
                   "eq": if eq.is_empty() { Value::Null } else { json!(eq) }})
        })
        .collect();
    let subset = prop.is_subset_of(&keys).map_or(Value::Null, |s| json!(s));
    let po_same = prop.partial_order_info.as_ref().map_or(Value::Null, |po| {
        let (a, b) = po.all_same_order();
        json!([a, b])
    });
    let si_mem: Vec<i64> = prop.sort_items.iter().map(|it| it.memory_usage()).collect();
    let hash = hexs(prop.hash_code());
    let clone = prop.clone_essential_fields();
    json!({
        "mem0": mem0, "hash": hash, "mem": prop.memory_usage(), "empty": prop.is_sort_item_empty(),
        "keep": prop.need_keep_order(), "desc": prop.get_sort_desc_for_keep_order(),
        "keep_items": sort_items_out(&prop.get_sort_items_for_keep_order()),
        "same": [same, desc], "allpart": prop.is_sort_item_all_for_partition(),
        "prefix": prop.is_prefix(&other), "fromschema": prop.all_cols_from_schema(&schema),
        "child": child, "flash": prop.is_flash_prop(), "clone_hash": hexs(clone.hash_code()),
        "clone_en": clone.can_add_enforcer, "tt": prop.task_tp.to_string(), "subset": subset,
        "mpp": if mpp.is_empty() { Value::Null } else { json!(mpp) }, "po_same": po_same,
        "si_mem": if si_mem.is_empty() { Value::Null } else { json!(si_mem) },
    })
}

fn stats_out(st: &StatsInfo) -> Value {
    let mut keys: Vec<&i64> = st.col_ndvs.keys().collect();
    keys.sort();
    let ndvs: Vec<Value> = keys
        .into_iter()
        .map(|k| json!([k, fo(st.col_ndvs[k])]))
        .collect();
    let groups: Vec<Value> = st
        .group_ndvs
        .iter()
        .map(|g| json!([g.cols, fo(g.ndv)]))
        .collect();
    json!({"rc": fo(st.row_count), "ndv": ndvs, "sv": st.stats_version, "groups": groups})
}

fn stats(v: &Value) -> Value {
    let mut st = StatsInfo {
        row_count: fl(&v["rc"]),
        col_ndvs: HashMap::new(),
        stats_version: v["sv"].as_u64().unwrap(),
        group_ndvs: Vec::new(),
    };
    for p in arr(&v["ndv"]) {
        st.col_ndvs.insert(s(&p[0]).parse().unwrap(), fl(&p[1]));
    }
    for g in arr(&v["groups"]) {
        st.group_ndvs.push(GroupNDV {
            cols: arr(&g["cols"]).iter().map(i).collect(),
            ndv: fl(&g["ndv"]),
        });
    }
    let cols: Vec<Arc<Column>> = arr(&v["cols"])
        .iter()
        .map(|id| Arc::new(int_col(i(id))))
        .collect();
    let group = StatsInfo::get_group_ndv4_cols(Some(&st), &cols)
        .map_or(Value::Null, |g| json!([g.cols, fo(g.ndv)]));
    let st = Arc::new(st);
    let sbe = st.scale_by_expect_cnt(fl(&v["expect"]));
    json!({
        "count": st.count(), "string": gs(st.to_string()), "scale": stats_out(&st.scale(fl(&v["factor"]))),
        "sbe": stats_out(&sbe), "sbe_same": Arc::ptr_eq(&sbe, &st), "group": group,
        "limit": stats_out(&derive_limit_stats(&st, fl(&v["limit"]))),
        "tostring": property::stats_info::to_string(&st.group_ndvs),
    })
}

fn cost_out(c: &CostVer2) -> Value {
    let mut out = json!({"cost": fo(c.get_cost())});
    if let Some(t) = c.get_trace() {
        let mut names: Vec<&String> = t.get_factor_costs().keys().collect();
        names.sort();
        let fs: Vec<Value> = names
            .into_iter()
            .map(|n| json!([n, fo(t.get_factor_costs()[n])]))
            .collect();
        out["formula"] = json!(t.get_formula());
        out["factors"] = json!(fs);
    }
    out
}

fn cost(p: &[Value]) -> Value {
    let trace = b(&p[0]);
    let mut option = new_default_plan_cost_option();
    if trace {
        option.with_cost_flag(CostFlagTrace);
    }
    let mut costs = Vec::new();
    let mut each = Vec::new();
    let mut fstrs = Vec::new();
    for (k, f) in arr(&p[1]).iter().enumerate() {
        let factor = CostVer2Factor {
            name: s(&f[0]),
            value: fl(&f[1]),
        };
        let name = s(&f[0]);
        let c = new_cost_ver2(Some(&option), &factor, fl(&f[2]), || format!("{name}*{k}"));
        each.push(cost_out(&c));
        costs.push(c);
        fstrs.push(gs(factor.to_string()));
    }
    costs.push(new_zero_cost_ver2(trace));
    let sum = sum_cost_ver2(&costs);
    let flags = arr(&p[4]);
    json!({
        "each": if each.is_empty() { Value::Null } else { json!(each) },
        "factor_str": if fstrs.is_empty() { Value::Null } else { json!(fstrs) },
        "sum": cost_out(&sum), "div": cost_out(&div_cost_ver2(&sum, fl(&p[2]))),
        "mul": cost_out(&mul_cost_ver2(&sum, fl(&p[3]))), "sum0": cost_out(&sum_cost_ver2(&[])),
        "zero": cost_out(&new_zero_cost_ver2(trace)),
        "has": has_cost_flag(flags[0].as_u64().unwrap(), flags[1].as_u64().unwrap()),
        "trace": trace_cost(Some(&option)), "trace_nil": trace_cost(None),
    })
}

fn handle_out(h: Result<Handle, tidb_base::Error>) -> Value {
    match h {
        Err(e) => json!({"err": String::from_utf8_lossy(&e.get_msg())}),
        Ok(Handle::Int(x)) => json!({"i": x.0}),
        Ok(Handle::Partition(p)) => {
            json!({"pid": p.partition_id, "h": handle_out(Ok((*p.handle).clone()))})
        }
        Ok(Handle::Common(c)) => json!({"c": hexs(c.encoded())}),
    }
}

fn cmp_out(r: Result<isize, tidb_base::Error>) -> Value {
    match r {
        Ok(c) => json!(c),
        Err(e) => json!({"err": String::from_utf8_lossy(&e.get_msg())}),
    }
}

fn try_json(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({"panic": true}))
}

fn resolved(hc: Result<HandleCols, tidb_base::Error>) -> Value {
    match hc {
        Err(e) => json!({"err": String::from_utf8_lossy(&e.get_msg())}),
        Ok(hc) => json!(
            (0..hc.num_cols())
                .map(|k| hc.get_col(k).unwrap().index)
                .collect::<Vec<_>>()
        ),
    }
}

fn handle_int(p: &[Value]) -> Value {
    let c = arr(&p[0]);
    let mut column = int_col(i(&c[0]));
    column.index = i(&c[1]) as isize;
    let (a, bb) = (datums(&p[1]), datums(&p[2]));
    let hc = new_int_handle_cols(Arc::new(column));
    let sc = new_stmt_ctx();
    let h = hc.build_handle_by_datums(&sc, &a);
    let ctors = collate::get_binary_collator_slice(a.len());
    let cmp = try_json(|| cmp_out(hc.compare(&a, &bb, &ctors, &DefaultStmtNoWarningContext)));
    let col1 = hc.get_col(1).map_or(Value::Null, |c| json!(c.unique_id));
    let row = mut_row_from_datums(&a);
    let row = row.to_row();
    let part_h = if a.len() >= 2 {
        let r = hc.build_partition_handle_from_index_row(&sc, row);
        handle_out(r.map(Handle::Partition))
    } else {
        Value::Null
    };
    let common = get_common_handle_datum(&hc, row).map_or(Value::Null, |d| json!(d.len()));
    json!({
        "h": handle_out(h), "cmp": cmp, "num": hc.num_cols(), "isint": hc.is_int(),
        "col0": hc.get_col(0).unwrap().unique_id, "col1": col1, "mem": hc.memory_usage(),
        "str": gs(hc.string_with_ctx(&*es::new_eval_context(vec![]), RedactLogDisable)),
        "ft": hc.get_fields_types().iter().map(|f| f.get_type()).collect::<Vec<_>>(),
        "resolved": resolved(hc.resolve_indices(&schema_of(&p[3]))),
        "clone_mem": hc.clone_cols().memory_usage(),
        "row_h": handle_out(hc.build_handle(&sc, row)),
        "idx_h": handle_out(hc.build_handle_from_index_row(&sc, row)),
        "part_h": part_h, "common": common,
    })
}

fn handle_common(p: &[Value]) -> Value {
    let mut tbl = TableInfo {
        is_common_handle: true,
        columns: Some(Vec::new()),
        ..Default::default()
    };
    let mut table_columns = Vec::new();
    for (k, tc) in arr(&p[0]).iter().enumerate() {
        let ft = field_type(tc[0].as_str().unwrap(), i(&tc[1]) as isize);
        tbl.columns.as_mut().unwrap().push(Arc::new(ColumnInfo {
            id: k as i64 + 1,
            offset: k as isize,
            name: CIStr::new(&format!("c{k}")),
            field_type: ft.clone(),
            ..Default::default()
        }));
        table_columns.push(Arc::new(Column {
            unique_id: 100 + k as i64,
            id: k as i64 + 1,
            index: k as isize,
            ret_type: ft,
            ..Default::default()
        }));
    }
    let idx = IndexInfo {
        primary: true,
        unique: true,
        columns: Some(
            arr(&p[1])
                .iter()
                .map(|ic| IndexColumn {
                    name: CIStr::new(&format!("c{}", i(&ic[0]))),
                    offset: i(&ic[0]) as isize,
                    length: i(&ic[1]) as isize,
                    ..Default::default()
                })
                .collect(),
        ),
        ..Default::default()
    };
    let (tbl, idx) = (Arc::new(tbl), Arc::new(idx));
    let (a, bb) = (datums(&p[2]), datums(&p[3]));
    let chc = new_common_handle_cols(Arc::clone(&tbl), Arc::clone(&idx), &table_columns);
    let hc = HandleCols::Common(chc.clone());
    let sc = new_stmt_ctx();
    let h = hc.build_handle_by_datums(&sc, &a);
    let ctors: Vec<_> = chc
        .get_columns()
        .iter()
        .map(|c| collate::get_collator(c.ret_type.get_collate()))
        .collect();
    let cmp = try_json(|| cmp_out(hc.compare(&a, &bb, &ctors, &DefaultStmtNoWarningContext)));
    let cols: Vec<i64> = (0..hc.num_cols())
        .map(|k| hc.get_col(k).unwrap().unique_id)
        .collect();
    let row = mut_row_from_datums(&a);
    let row = row.to_row();
    let mut idx_row = vec![new_int_datum(7)];
    for c in chc.get_columns() {
        idx_row.push(a[c.index as usize].clone());
    }
    let mut p_row = idx_row.clone();
    p_row.push(new_int_datum(42));
    let ir = mut_row_from_datums(&idx_row);
    let pr = mut_row_from_datums(&p_row);
    let common: Vec<Value> = get_common_handle_datum(&hc, row)
        .unwrap()
        .iter()
        .map(|d| match d.kind() {
            KindInt64 => json!(d.get_int64().to_string()),
            KindString => gs(d.get_string().to_vec()),
            k => panic!("kind {k}"),
        })
        .collect();
    let wo = new_common_handles_cols_without_cols_align(tbl, idx, chc.get_columns().to_vec());
    json!({
        "h": handle_out(h), "cmp": cmp, "num": hc.num_cols(), "isint": hc.is_int(), "cols": cols,
        "mem": hc.memory_usage(),
        "str": gs(hc.string_with_ctx(&*es::new_eval_context(vec![]), RedactLogDisable)),
        "ft": hc.get_fields_types().iter().map(|f| f.get_type()).collect::<Vec<_>>(),
        "resolved": resolved(hc.resolve_indices(&schema_of(&p[4]))),
        "clone_mem": hc.clone_cols().memory_usage(),
        "row_h": handle_out(hc.build_handle(&sc, row)),
        "idx_h": handle_out(hc.build_handle_from_index_row(&sc, ir.to_row())),
        "part_h": handle_out(hc.build_partition_handle_from_index_row(&sc, pr.to_row()).map(Handle::Partition)),
        "common": common, "wo_num": wo.num_cols(),
    })
}

fn split_cor(p: &[Value]) -> Value {
    let ctx = es::new_expr_context(vec![]);
    let idx_cols: Vec<Arc<Column>> = arr(&p[0])
        .iter()
        .map(|id| Arc::new(int_col(i(id))))
        .collect();
    let lens: Vec<isize> = arr(&p[1]).iter().map(|l| i(l) as isize).collect();
    let filters: Vec<ExprRef> = arr(&p[2])
        .iter()
        .map(|f| {
            let column = Expr::Column(int_col(i(&f[1]))).into_ref();
            let (fname, other) = match f[0].as_str().unwrap() {
                "const" => (ast_funcs::EQ, int_const(1)),
                "lt" => (ast_funcs::LT, int_const(1)),
                "cor" => (
                    ast_funcs::EQ,
                    Expr::CorrelatedColumn(CorrelatedColumn::new(int_col(999))).into_ref(),
                ),
                "col" => (ast_funcs::EQ, Expr::Column(int_col(998)).into_ref()),
                k => panic!("bad filter {k}"),
            };
            let args = if b(&f[2]) {
                vec![other, column]
            } else {
                vec![column, other]
            };
            new_function_internal(&*ctx, fname, new_field_type(TypeLonglong), args).unwrap()
        })
        .collect();
    let path = AccessPath {
        idx_cols,
        idx_col_lens: lens,
        table_filters: filters.clone(),
        access_conds: filters.clone(),
        ..Default::default()
    };
    let (access, remained) = path.split_cor_col_access_cond_from_filters(&*ctx, i(&p[3]) as isize);
    let pos = |es: &[ExprRef]| -> Vec<usize> {
        es.iter()
            .filter_map(|e| filters.iter().position(|f| Arc::ptr_eq(f, e)))
            .collect()
    };
    let table_path = AccessPath {
        is_int_handle_path: true,
        access_conds: filters.clone(),
        ..Default::default()
    };
    json!({
        "access": pos(&access), "remained": pos(&remained),
        "skip": !ctx.get_plan_cache_tracker().use_cache(),
        "c2l": col2len_out(&path.get_col2_len_from_access_conds(&*ctx)),
        "c2l_table": col2len_out(&table_path.get_col2_len_from_access_conds(&*ctx)),
    })
}

fn int_const(v: i64) -> ExprRef {
    Expr::Constant(tidb_expression::Constant::new(
        new_int_datum(v),
        new_field_type(TypeLonglong),
    ))
    .into_ref()
}

fn by_items_in(v: &Value) -> Vec<ByItems> {
    arr(v)
        .iter()
        .map(|it| ByItems {
            expr: Expr::Column(int_col(i(&it[0]))).into_ref(),
            desc: b(&it[1]),
        })
        .collect()
}

fn by_items(p: &[Value]) -> Value {
    let (items, other) = (by_items_in(&p[0]), by_items_in(&p[1]));
    let ctx = es::new_eval_context(vec![]);
    let mut strs = Vec::new();
    let mut eqs = Vec::new();
    let mut mem = Vec::new();
    for (k, it) in items.iter().enumerate() {
        strs.push(gs(it.string_with_ctx(&*ctx, RedactLogDisable)));
        mem.push(it.memory_usage());
        if k < other.len() {
            eqs.push(it.equal(&*ctx, &other[k]));
        }
        assert!(it.clone_item().equal(&*ctx, it));
    }
    let none_if_empty = |v: Value| {
        if v.as_array().unwrap().is_empty() {
            Value::Null
        } else {
            v
        }
    };
    json!({"str": none_if_empty(json!(strs)), "mem": none_if_empty(json!(mem)),
           "eq": none_if_empty(json!(eqs)), "all": gs(stringify_by_items_with_ctx(&*ctx, &items))})
}

fn misc(p: &[Value]) -> Value {
    match p[0].as_str().unwrap() {
        "enc32" => {
            let mut out = vec![0xab];
            encode_int_as_uint32(&mut out, i(&p[1]) as isize);
            hexs(&out)
        }
        "maxsort" => {
            let mk = |v: &Value| -> Vec<Arc<Column>> {
                arr(v).iter().map(|id| Arc::new(int_col(i(id)))).collect()
            };
            ints_out(&get_max_sort_prefix(&mk(&p[1]), &mk(&p[2])))
        }
        "qtr" => {
            let t = arr(&p[1]);
            let tr = QueryTimeRange {
                from: unix(i(&t[0]), i(&t[1])),
                to: unix(i(&t[2]), i(&t[3])),
            };
            json!([tr.condition(), tr.memory_usage()])
        }
        "exch" => json!(MPPPartitionType(i(&p[1]) as isize).to_exchange_type() as i32),
        "collid" => json!([
            property::get_collate_id_by_name_for_partition(p[1].as_str().unwrap()),
            property::get_collate_name_by_id_for_partition(i(&p[2]) as i32)
        ]),
        "explaincols" => {
            let mc = mpp_cols(&p[1]);
            let schema = schema_of(&p[2]);
            let mut res = Vec::new();
            for c in &mc {
                match c.resolve_indices(&schema) {
                    Err(e) => res.push(json!(String::from_utf8_lossy(&e.get_msg()))),
                    Ok(r) => res.push(json!([r.col.index, r.collate_id])),
                }
                assert!(c.clone_column().equal(c));
            }
            json!([
                gs(property::explain_column_list(
                    &*es::new_eval_context(vec![]),
                    &mc
                )),
                if res.is_empty() {
                    Value::Null
                } else {
                    json!(res)
                }
            ])
        }
        "match" => json!(PhysicalPropMatchResult(i(&p[1]) as isize).matched()),
        "sortitem" => json!(
            sort_items(&p[1])
                .iter()
                .map(|it| json!([gs(it.clone_item().to_string()), it.memory_usage()]))
                .collect::<Vec<_>>()
        ),
        "tasktype" => json!(TaskType(i(&p[1]) as isize).to_string()),
        w => panic!("bad misc {w}"),
    }
}

fn exec_op(op: &str, p: &[Value]) -> Value {
    collate::set_new_collation_enabled_for_test(true);
    match op {
        "col2len" => {
            let (r, ok) = compare_col2_len(&col2len(&p[0]), &col2len(&p[1]));
            json!([r, ok])
        }
        "only_point" => {
            let n = i(&p[1]);
            let path = AccessPath {
                is_int_handle_path: b(&p[0]),
                ranges: ranges(&p[2]),
                index: (n >= 0).then(|| {
                    Arc::new(IndexInfo {
                        columns: Some((0..n).map(|_| IndexColumn::default()).collect()),
                        ..Default::default()
                    })
                }),
                ..Default::default()
            };
            json!(path.only_point_range(&DefaultStmtNoWarningContext))
        }
        "full_scan" => {
            let mut ft = new_field_type(TypeLonglong);
            ft.add_flag(PriKeyFlag);
            if b(&p[2]) {
                ft.add_flag(UnsignedFlag);
            }
            let tbl = TableInfo {
                pk_is_handle: b(&p[1]),
                columns: Some(vec![Arc::new(ColumnInfo {
                    name: CIStr::new("a"),
                    field_type: ft,
                    ..Default::default()
                })]),
                ..Default::default()
            };
            let path = AccessPath {
                is_int_handle_path: b(&p[0]),
                ranges: ranges(&p[3]),
                ..Default::default()
            };
            json!(path.is_full_scan_range(&tbl))
        }
        "path_flags" => {
            let path = AccessPath {
                is_int_handle_path: b(&p[0]),
                is_common_handle_path: b(&p[1]),
                store_type: StoreType(i(&p[3]) as u8),
                index: b(&p[2]).then(|| {
                    Arc::new(IndexInfo {
                        mv_index: b(&p[4]),
                        condition_expr_string: s(&p[5]),
                        ..Default::default()
                    })
                }),
                ..Default::default()
            };
            json!([
                path.is_table_path(),
                path.is_tikv_table_path(),
                path.is_tiflash_simple_table_path(),
                path.is_undetermined(),
                path.is_index_join_unapplicable()
            ])
        }
        "index_cols" => index_cols(p),
        "prop" => prop(p),
        "stats" => stats(&p[0]),
        "cost" => cost(p),
        "handle_int" => handle_int(p),
        "handle_common" => handle_common(p),
        "split_cor" => split_cor(p),
        "byitems" => by_items(p),
        "misc" => misc(p),
        o => panic!("unknown op {o}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "planutil");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let op = rec["op"].as_str().unwrap().to_string();
        let got = catch_unwind(AssertUnwindSafe(|| exec_op(&op, &args)));
        let go_panicked = rec["err"]["name"] == "panic";
        match got {
            Err(_) if go_panicked => {}
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
                fails.push(format!("{line}\n    rust panicked: {msg:?}"));
            }
            Ok(got) if go_panicked => fails.push(format!("{line}\n    rust returned {got}")),
            Ok(got) => {
                if got != rec["out"] {
                    fails.push(format!("{line}\n    got {got}"));
                }
            }
        }
    }
    std::panic::set_hook(prev);
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
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_planutil() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_PLANUTIL_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
