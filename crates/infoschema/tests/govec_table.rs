//! Replays the govec `table` vectors (tidb-rs-tracker `govec/domains/table.go`): block
//! D06.4 part 1, the read path of pkg/table and pkg/table/tables, against Go TiDB v8.5.8.
//!
//! Every record's `out` must be equal as JSON; errors match by code and message, except
//! parser errors (`ErrParse`), whose text is the Rust parser's (D06.4 spec §4): their code
//! must match. `{"panic": true}` sub-results must panic in Rust too.
//!
//! `GOVEC_TABLE_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use protobuf::Message;
use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{SQLWarn, WarnHandler, new_static_warn_handler};
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_base::mysql::SQLMode;
use tidb_infoschema::autoid::Allocators;
use tidb_infoschema::table::{self, ColumnRef, Table, column};
use tidb_infoschema::tables;
use tidb_kv::key::{Handle, IntHandle, new_common_handle};
use tidb_model::TableInfo;
use tidb_parser::types::FieldType;
use tidb_sessionctx::errctx::{Level, LevelMap};
use tidb_sessionctx::exprstatic as es;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::context::Flags;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::json::BinaryJSON;
use tidb_types::set_type::Set;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::parse_vector_float32;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/table.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

// ---------------------------------------------------------------- values (as govec_tablecodec.rs)

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// vec.GoString's JSON form.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex(b) }),
    }
}

fn ungs(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => unhex(m["hex"].as_str().unwrap()),
        _ => panic!("not a GoString: {v}"),
    }
}

fn hex_opt(v: &Value) -> Option<Vec<u8>> {
    if v.is_null() {
        None
    } else {
        Some(unhex(v.as_str().unwrap()))
    }
}

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
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

fn bits_f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

fn datum_in(v: &Value) -> Datum {
    let p = v.as_array().unwrap();
    let k = p[0].as_u64().unwrap() as u8;
    let u = |i: usize| p[i].as_u64().unwrap();
    let i = |i: usize| p[i].as_i64().unwrap();
    let st = |i: usize| p[i].as_str().unwrap().to_owned();
    let mut d = Datum::default();
    match k {
        dm::KindNull => d.set_null(),
        dm::KindInt64 => d.set_int64(i(1)),
        dm::KindUint64 => d.set_uint64(u(1)),
        dm::KindFloat32 => d.set_float32(bits_f(&p[1]) as f32),
        dm::KindFloat64 => d.set_float64(bits_f(&p[1])),
        dm::KindString => d.set_string(ungs(&p[1]), st(2)),
        dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
        dm::KindBinaryLiteral => {
            d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
        }
        dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
        dm::KindMysqlDecimal => d.set_mysql_decimal(dec_of(p[1].as_str().unwrap())),
        dm::KindMysqlDuration => d.set_mysql_duration(Duration {
            duration: i(1),
            fsp: i(2) as isize,
        }),
        dm::KindMysqlEnum => d.set_mysql_enum(
            Enum {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlSet => d.set_mysql_set(
            Set {
                value: u(1),
                name: ungs(&p[2]),
            },
            st(3),
        ),
        dm::KindMysqlTime => d.set_mysql_time(Time::from_raw(CoreTime(u(1)))),
        dm::KindMysqlJSON => d.set_mysql_json(BinaryJSON::owned(
            u(1) as u8,
            hex_opt(&p[2]).unwrap_or_default(),
        )),
        dm::KindVectorFloat32 => {
            d.set_vector_float32(parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap())
        }
        _ => panic!("datum_in: kind {k}"),
    }
    d
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

fn datum_outs(ds: &[Datum]) -> Value {
    Value::Array(ds.iter().map(datum_out).collect())
}

fn ft_in(v: &Value) -> FieldType {
    let p = v.as_array().unwrap();
    let mut ft = new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p();
    if !p[6].is_null() {
        ft.set_elems(Some(p[6].as_array().unwrap().iter().map(ungs).collect()));
    }
    ft
}

fn loc_of(v: &Value) -> Arc<Location> {
    if v.is_null() {
        utc()
    } else {
        fixed_zone("Z", v.as_i64().unwrap() as i32)
    }
}

fn handle_of(v: &Value) -> Handle {
    match v["t"].as_str().unwrap() {
        "int" => Handle::Int(IntHandle(v["v"].as_i64().unwrap_or(0))),
        "common" => Handle::Common(Arc::new(
            new_common_handle(unhex(v["enc"].as_str().unwrap_or(""))).unwrap(),
        )),
        t => panic!("bad spec {t}"),
    }
}

fn warns(ws: &[SQLWarn]) -> Value {
    Value::Array(
        ws.iter()
            .map(|w| json!([&*w.level, gs(&w.err.error())]))
            .collect(),
    )
}

fn try_(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

type Ctxs = (Arc<es::ExprContext>, Arc<es::EvalContext>);

/// tblCtx: [castCtx, loc]; castCtx is [typeFlags, [7]errLevels, maxAllowedPacket, sqlMode,
/// now (unix ns)].
fn ctx_of(v: &Value) -> Ctxs {
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
    let ctx = es::new_expr_context(vec![es::with_eval_ctx(Arc::clone(&ev))]);
    (ctx, ev)
}

fn tbl_in(v: &Value) -> Arc<TableInfo> {
    let mut t = TableInfo::default();
    t.unmarshal_json(v.as_str().unwrap().as_bytes()).unwrap();
    Arc::new(t)
}

fn datum_or_err(r: Result<Datum, Error>) -> Value {
    match r {
        Ok(d) => json!({ "d": datum_out(&d) }),
        Err(e) => json!({ "err": gs(&e.error()), "d": datum_out(&Datum::default()) }),
    }
}

fn datum_or_err2(d: Datum, r: Result<(), Error>) -> Value {
    match r {
        Ok(()) => json!({ "d": datum_out(&d) }),
        Err(e) => json!({ "err": gs(&e.error()), "d": datum_out(&d) }),
    }
}

fn cols_out(cols: &[ColumnRef]) -> Value {
    Value::Array(cols.iter().map(|c| json!(c.offset)).collect())
}

fn ids_out(ids: Option<Vec<i64>>) -> Value {
    match ids {
        None => Value::Null,
        Some(v) => json!(v),
    }
}

// ---------------------------------------------------------------- ops

fn table_summary(t: &dyn Table) -> Value {
    let meta = t.meta();
    let mut cols_info = vec![];
    for c in t.deletable_cols() {
        let mut x = vec![
            json!(c.offset),
            json!(c.string()),
            json!(c.is_pk_handle_column(meta)),
            json!(c.is_common_handle_column(meta)),
            json!(Arc::ptr_eq(c.to_info(), &c.info)),
        ];
        if let Some(g) = &c.generated_expr {
            x.push(json!(g.clone_node().is_ok()));
            x.push(json!(g.internal().is_ok()));
        }
        cols_info.push(Value::Array(x));
    }
    let idx: Vec<Value> = t
        .indices()
        .iter()
        .map(|i| json!([i.meta().name.o, Arc::ptr_eq(i.table_meta(), meta)]))
        .collect();
    let tp = t.type_();
    let mut out = serde_json::Map::new();
    out.insert("cols".into(), cols_out(t.cols()));
    out.insert("visible".into(), cols_out(t.visible_cols()));
    out.insert("hidden".into(), cols_out(t.hidden_cols()));
    out.insert("writable".into(), cols_out(t.writable_cols()));
    out.insert("deletable".into(), cols_out(t.deletable_cols()));
    out.insert(
        "full".into(),
        cols_out(t.full_hidden_cols_and_visible_cols()),
    );
    out.insert(
        "col_info".into(),
        if cols_info.is_empty() {
            Value::Null
        } else {
            Value::Array(cols_info)
        },
    );
    out.insert(
        "indices".into(),
        if idx.is_empty() {
            Value::Null
        } else {
            Value::Array(idx)
        },
    );
    out.insert("del_idx".into(), json!(t.deletable_indices().len()));
    // A partitioned table's record prefix is a debug_assert in part 1 (VS1-21: the planner
    // refuses partitioned tables): the logical table's prefix is computed here instead.
    let rp = if t.get_partitioned_table().is_some() {
        tidb_tablecodec::tablecodec::gen_table_record_prefix(meta.id)
    } else {
        t.record_prefix().clone()
    };
    out.insert("rp".into(), json!(hex(&rp.0)));
    out.insert("ip".into(), json!(hex(&t.index_prefix().0)));
    out.insert(
        "type".into(),
        json!([
            tp.is_normal_table(),
            tp.is_virtual_table(),
            tp.is_cluster_table()
        ]),
    );
    out.insert("part".into(), json!(t.get_partitioned_table().is_some()));
    out.insert("cached".into(), json!(t.as_cached_table().is_some()));
    out.insert("allocs".into(), json!(t.allocators().allocs.len()));
    if let Some(pt) = t.as_physical_table() {
        out.insert("pid".into(), json!(pt.get_physical_id()));
    }
    // Go: only a plain *TableCommon (not the partitioned / cached wrappers).
    if t.get_partitioned_table().is_none()
        && t.as_cached_table().is_none()
        && let Some(tc) = t.as_any().downcast_ref::<tables::TableCommon>()
    {
        let cp = tc.copy();
        out.insert("copy_cols".into(), json!(cp.cols().len()));
        out.insert(
            "rk".into(),
            json!(hex(&tc.record_key(&Handle::Int(IntHandle(7))).0)),
        );
    }
    if let Some(pk) = tables::find_primary_index(meta) {
        out.insert("pk".into(), json!(pk.name.o));
    }
    out.insert(
        "pk_ids".into(),
        try_(|| ids_out(tables::try_get_common_pk_column_ids(meta))),
    );
    out.insert(
        "prefix_ids".into(),
        try_(|| ids_out(tables::primary_prefix_column_ids(meta))),
    );
    out.insert(
        "pk_cols".into(),
        try_(|| match tables::try_get_common_pk_columns(t) {
            Some(c) => cols_out(&c),
            None => Value::Null,
        }),
    );
    let by: Vec<Value> = meta
        .columns
        .iter()
        .flatten()
        .map(|c| match tables::find_index_by_col_name(t, &c.name.l) {
            Some(i) => json!(i.meta().name.o),
            None => Value::Null,
        })
        .collect();
    out.insert(
        "by_col".into(),
        if by.is_empty() {
            Value::Null
        } else {
            Value::Array(by)
        },
    );
    Value::Object(out)
}

fn find_out(cols: Option<Vec<ColumnRef>>) -> Value {
    match cols {
        None => Value::Null,
        Some(c) => Value::Array(
            c.iter()
                .map(|c| json!([c.id, c.name.o, c.offset]))
                .collect(),
        ),
    }
}

fn map_out(m: &HashMap<i64, Datum>) -> Value {
    let mut ids: Vec<_> = m.keys().copied().collect();
    ids.sort();
    Value::Array(
        ids.iter()
            .map(|id| json!([id, datum_out(&m[id])]))
            .collect(),
    )
}

/// `(out, err)` of one record.
fn exec(op: &str, a: &[Value]) -> (Value, Option<Error>) {
    match op {
        "zero_value" => {
            let ci = tidb_model::ColumnInfo {
                field_type: ft_in(&a[0]),
                ..Default::default()
            };
            (datum_out(&column::get_zero_value(&ci)), None)
        }
        "optional_fsp" => (json!(column::optional_fsp(&ft_in(&a[0]))), None),
        "cast_column" => {
            let (ctx, ev) = ctx_of(&a[0]);
            let ci = tidb_model::ColumnInfo {
                field_type: ft_in(&a[2]),
                name: tidb_parser::model::CIStr::new(a[3].as_str().unwrap()),
                ..Default::default()
            };
            let (d, r) = column::cast_column_value(
                &*ctx,
                datum_in(&a[1]),
                &ci,
                a[4].as_bool().unwrap(),
                a[5].as_bool().unwrap(),
            );
            (
                json!({ "r": datum_or_err2(d, r), "w": warns(&ev.truncate_warnings(0)) }),
                None,
            )
        }
        "default_value" => {
            let (ctx, ev) = ctx_of(&a[0]);
            let tbl = tbl_in(&a[1]);
            let off = a[2].as_u64().unwrap() as usize;
            let ci = &tbl.columns.as_ref().unwrap()[off];
            let r = try_(|| match a[3].as_u64().unwrap() {
                0 => datum_or_err(column::get_col_default_value(&*ctx, ci)),
                1 => datum_or_err(column::get_col_origin_default_value(&*ctx, ci)),
                2 => datum_or_err(
                    column::get_col_origin_default_value_without_strict_sql_mode(&*ctx, ci),
                ),
                _ => {
                    let col = column::to_column(Arc::new(ci.clone()));
                    let mut vals = vec![Datum::default(); tbl.columns.as_ref().unwrap().len()];
                    let d1 = tables::get_col_default_value(&*ctx, &col, &mut vals);
                    let d2 = tables::get_col_default_value(&*ctx, &col, &mut vals);
                    json!([datum_or_err(d1), datum_or_err(d2)])
                }
            });
            (
                json!({ "r": r, "w": warns(&ev.truncate_warnings(0)) }),
                None,
            )
        }
        "time_value" => {
            let (ctx, ev) = ctx_of(&a[0]);
            let tz = if a[4].is_null() {
                None
            } else {
                Some(loc_of(&a[4]))
            };
            let r = tidb_expression::helper::get_time_value(
                &*ctx,
                tidb_expression::helper::TimeValueArg::Str(a[1].as_str().unwrap()),
                a[2].as_u64().unwrap() as u8,
                a[3].as_i64().unwrap() as isize,
                tz,
            );
            (
                json!({ "r": datum_or_err(r), "w": warns(&ev.truncate_warnings(0)) }),
                None,
            )
        }
        "table_from_meta" => {
            let tbl = tbl_in(&a[0]);
            if a[1].as_bool().unwrap() {
                return match table::mock_table_from_meta(tbl) {
                    None => (json!({ "nil": true }), None),
                    Some(t) => (table_summary(&*t), None),
                };
            }
            match tables::table_from_meta(Allocators::default(), tbl) {
                Ok(t) => (table_summary(&*t), None),
                Err(e) => (Value::Null, Some(e)),
            }
        }
        "decode_row" => {
            let (ctx, ev) = ctx_of(&a[0]);
            let tbl = tbl_in(&a[1]);
            let t = match tables::table_from_meta(Allocators::default(), tbl.clone()) {
                Ok(t) => t,
                Err(e) => return (Value::Null, Some(e)),
            };
            let h = handle_of(&a[2]);
            let cols: Vec<Option<ColumnRef>> = a[3]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    let o = o.as_i64().unwrap();
                    (o >= 0).then(|| t.cols()[o as usize].clone())
                })
                .collect();
            let value = unhex(a[4].as_str().unwrap());
            let decode = || {
                try_(
                    || match tables::decode_raw_row_data(&*ctx, &tbl, &h, &cols, &value) {
                        Ok((v, m)) => json!({ "v": datum_outs(&v), "m": map_out(&m) }),
                        Err(e) => json!({ "err": gs(&e.error()) }),
                    },
                )
            };
            let r = decode();
            let w = warns(&ev.truncate_warnings(0));
            // The column field types are borrowed for one decode call only (#229): decoding
            // the same row again gives the same result.
            assert_eq!(decode(), r, "decode_raw_row_data twice");
            ev.truncate_warnings(0);
            (json!({ "r": r, "w": w }), None)
        }
        "find_cols" => {
            let tbl = tbl_in(&a[0]);
            let cols: Vec<ColumnRef> = tbl
                .columns
                .iter()
                .flatten()
                .map(|c| Arc::new(column::to_column(Arc::new(c.clone()))))
                .collect();
            let names: Vec<String> = a[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect();
            let pk = a[2].as_bool().unwrap();
            let (c1, s1) = column::find_cols(&cols, &names, pk);
            let (c2, o2) = column::find_columns(&cols, &names, pk);
            (json!([find_out(c1), s1, find_out(c2), o2]), None)
        }
        "restored" => {
            let tbl = tbl_in(&a[0]);
            let row: Vec<Datum> = a[1].as_array().unwrap().iter().map(datum_in).collect();
            let mut m = HashMap::new();
            if a[2].as_bool().unwrap() {
                for (i, c) in tbl.columns.iter().flatten().enumerate() {
                    m.insert(c.id, row[i].clone());
                }
            }
            let idx = &tbl.indices.as_ref().unwrap()[a[3].as_u64().unwrap() as usize];
            let r =
                try_(
                    || match tables::try_get_handle_restored_data_wrapper(&tbl, &row, &m, idx) {
                        None => Value::Null,
                        Some(rs) => datum_outs(&rs),
                    },
                );
            (r, None)
        }
        "can_skip" => {
            let tbl = tbl_in(&a[0]);
            let ci = &tbl.columns.as_ref().unwrap()[a[1].as_u64().unwrap() as usize];
            let col = column::to_column(Arc::new(ci.clone()));
            let d = datum_in(&a[2]);
            (try_(|| json!(tables::can_skip(&tbl, &col, &d))), None)
        }
        "table_scan" => {
            let (ctx, ev) = ctx_of(&a[0]);
            let tbl = tbl_in(&a[1]);
            let extra = tidb_model::new_extra_handle_col_info();
            // A nil Go slice is null.
            let cis: Vec<&tidb_model::ColumnInfo> = a[2]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| {
                    let o = o.as_i64().unwrap();
                    if o < 0 {
                        &extra
                    } else {
                        &tbl.columns.as_ref().unwrap()[o as usize]
                    }
                })
                .collect();
            let mut ts = tables::build_table_scan_from_infos(&tbl, &cis, a[3].as_bool().unwrap());
            let mut res = serde_json::Map::new();
            res.insert("scan".into(), json!(hex(&ts.write_to_bytes().unwrap())));
            let mut pb_cols = ts.take_columns().into_vec();
            if let Err(e) = tables::set_pb_columns_default_value(&*ctx, &mut pb_cols, &cis) {
                res.insert("err".into(), gs(&e.error()));
            }
            let dv: Vec<Value> = pb_cols
                .iter()
                .map(|c| {
                    if c.has_default_val() {
                        json!(hex(c.get_default_val()))
                    } else {
                        Value::Null
                    }
                })
                .collect();
            res.insert(
                "dv".into(),
                if dv.is_empty() {
                    Value::Null
                } else {
                    Value::Array(dv)
                },
            );
            res.insert("w".into(), warns(&ev.truncate_warnings(0)));
            (Value::Object(res), None)
        }
        _ => panic!("unknown op {op}"),
    }
}

// ---------------------------------------------------------------- replay

/// Whether two messages are parser errors (`[parser:1064]...`): the text after the code is
/// the parser's own (D06.4 spec §4).
fn parse_errors(a: &Value, b: &Value) -> bool {
    let p = |v: &Value| v.as_str().is_some_and(|s| s.starts_with("[parser:1064]"));
    p(a) && p(b)
}

/// JSON equality where two parser-error messages compare equal.
fn same(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            g.len() == w.len()
                && g.iter()
                    .all(|(k, v)| w.get(k).is_some_and(|wv| same(v, wv)))
        }
        (Value::Array(g), Value::Array(w)) => {
            g.len() == w.len() && g.iter().zip(w).all(|(a, b)| same(a, b))
        }
        _ => got == want || parse_errors(got, want),
    }
}

fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
    if want_err["name"] == "panic" {
        return match got {
            Err(_) => None,
            Ok((out, err)) => Some(format!("Go panics, Rust returned {out} / {err:?}")),
        };
    }
    let (out, err) = match got {
        Ok(v) => v,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            return Some(format!("Rust panicked: {msg}"));
        }
    };
    let want_out = &rec["out"];
    if !same(&out, want_out) {
        return Some(format!("out: got {out}\n    want {want_out}"));
    }
    let got_err = err
        .as_ref()
        .map(|e| json!({ "code": e.code(), "msg": gs(&e.error()) }));
    let want_err_cmp = if want_err.is_null() {
        None
    } else {
        Some(
            json!({ "code": want_err.get("code").cloned().unwrap_or(json!(0)), "msg": want_err["msg"] }),
        )
    };
    match (&got_err, &want_err_cmp) {
        (Some(g), Some(w)) if same(g, w) => None,
        (None, None) => None,
        _ => Some(format!("err: got {got_err:?}, want {want_err_cmp:?}")),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "table");
    tidb_types::collate::set_new_collation_enabled_for_test(true);
    let prev = std::panic::take_hook();
    if std::env::var("GOVEC_PANIC").is_err() {
        std::panic::set_hook(Box::new(|_| {}));
    }
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            let cut = line
                .char_indices()
                .nth(600)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            fails.push(format!("{}\n    {why}", &line[..cut]));
        }
    }
    std::panic::set_hook(prev);
    let show = std::env::var("GOVEC_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let shown: Vec<_> = fails.iter().take(show).map(|s| s.as_str()).collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_table() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_TABLE_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
