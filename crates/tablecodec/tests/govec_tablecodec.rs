//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/tablecodec.jsonl.gz`
//! (tracker `govec`, domain `tablecodec`: block D01.11, pkg/tablecodec + pkg/util/rowcodec,
//! read path) against the Rust port.
//!
//! Every record's `out` must be equal as JSON; errors must match by code and message;
//! `{"panic": true}` sub-results must panic in Rust too; `{"fail": true}` (a decoder whose
//! failing column depends on Go's map order) must be an error or a panic.
//!
//! Extra vector files (other seeds, e.g. tracker CI's `govec tablecodec -seed N`) are
//! replayed too when `GOVEC_TABLECODEC_FILES` lists them (comma-separated paths).

#![allow(clippy::needless_range_loop)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_kv::key::{Handle, IntHandle, KeyRange, new_common_handle, new_partition_handle};
use tidb_kv::kv::{new_non_partitioned_key_ranges, new_partitioned_key_ranges};
use tidb_model::{ColumnInfo, IndexColumn, TableInfo};
use tidb_parser::types::FieldType;
use tidb_tablecodec as tc;
use tidb_tablecodec::rowcodec::row::Row;
use tidb_tablecodec::rowcodec::{self, ColData, ColInfo, RowData};
use tidb_tablecodec::rowindexcodec::get_key_kind;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/tablecodec.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

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

fn hexv(v: &Value) -> Vec<u8> {
    hex_opt(v).unwrap_or_default()
}

fn err_msg(e: &Result<(), Error>) -> Value {
    match e {
        Ok(()) => Value::Null,
        Err(e) => gs(&e.error()),
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
        dm::KindMinNotNull => d.set_min_not_null(),
        dm::KindMaxValue => d = dm::max_value_datum(),
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
        dm::KindRaw => d.set_raw(hex_opt(&p[1])),
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

fn loc_arg(v: &Value) -> Arc<Location> {
    if v.is_null() {
        utc()
    } else {
        fixed_zone("Z", v.as_i64().unwrap() as i32)
    }
}

fn loc_opt(v: &Value) -> Option<Arc<Location>> {
    if v.is_null() {
        None
    } else {
        Some(fixed_zone("Z", v.as_i64().unwrap() as i32))
    }
}

fn datums_arg(v: &Value) -> Vec<Datum> {
    v.as_array().unwrap().iter().map(datum_in).collect()
}

/// `codecTry`: a panic is `{"panic": true}`.
fn try_(f: impl FnOnce() -> Value) -> Value {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => json!({ "panic": true }),
    }
}

// ---------------------------------------------------------------- domain values

fn spec_json(h: &Handle) -> String {
    match h {
        Handle::Int(x) => {
            if x.0 == 0 {
                r#"{"t":"int"}"#.to_owned()
            } else {
                format!(r#"{{"t":"int","v":{}}}"#, x.0)
            }
        }
        Handle::Common(c) => {
            let mut offs = vec![];
            let mut end = 0u16;
            for i in 0..c.num_cols() {
                end += c.encoded_col(i).len() as u16;
                offs.push(end.to_string());
            }
            let mut s = r#"{"t":"common""#.to_owned();
            if !c.encoded().is_empty() {
                s += &format!(r#","enc":"{}""#, hex(c.encoded()));
            }
            if !offs.is_empty() {
                s += &format!(r#","offs":[{}]"#, offs.join(","));
            }
            s + "}"
        }
        Handle::Partition(p) => {
            let mut s = r#"{"t":"part""#.to_owned();
            if p.partition_id != 0 {
                s += &format!(r#","pid":{}"#, p.partition_id);
            }
            s + &format!(r#","h":{}}}"#, spec_json(&p.handle))
        }
    }
}

fn handle_out(h: &Handle) -> Value {
    serde_json::from_str(&spec_json(h)).unwrap()
}

fn handle_of(v: &Value) -> Handle {
    match v["t"].as_str().unwrap() {
        "int" => Handle::Int(IntHandle(v["v"].as_i64().unwrap_or(0))),
        "common" => Handle::Common(Arc::new(
            new_common_handle(unhex(v["enc"].as_str().unwrap_or(""))).unwrap(),
        )),
        "part" => Handle::Partition(new_partition_handle(
            v["pid"].as_i64().unwrap_or(0),
            handle_of(&v["h"]),
        )),
        t => panic!("bad spec {t}"),
    }
}

fn handle_opt(v: &Value) -> Option<Handle> {
    if v.is_null() {
        None
    } else {
        Some(handle_of(v))
    }
}

fn col_infos(v: &Value) -> Vec<ColInfo> {
    v.as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|q| ColInfo {
            id: q[0].as_i64().unwrap(),
            is_pk_handle: q[1].as_bool().unwrap(),
            virtual_gen_col: q[2].as_bool().unwrap(),
            ft: Arc::new(ft_in(&q[3])),
        })
        .collect()
}

fn cols_map(v: &Value) -> HashMap<i64, Arc<FieldType>> {
    let mut m = HashMap::new();
    for q in v.as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        m.insert(q[0].as_i64().unwrap(), Arc::new(ft_in(&q[1])));
    }
    m
}

fn map_out(m: &HashMap<i64, Datum>) -> Value {
    let mut ids: Vec<&i64> = m.keys().collect();
    ids.sort();
    Value::Array(
        ids.into_iter()
            .map(|id| json!([id, datum_out(&m[id])]))
            .collect(),
    )
}

fn bytes_out(bs: &[impl AsRef<[u8]>]) -> Value {
    Value::Array(
        bs.iter()
            .map(|b| {
                let b = b.as_ref();
                if b.is_empty() {
                    Value::Null
                } else {
                    json!(hex(b))
                }
            })
            .collect(),
    )
}

/// Go `[][]byte(nil)` marshals as null.
fn bytes_out_or_null(bs: &[impl AsRef<[u8]>]) -> Value {
    if bs.is_empty() {
        Value::Null
    } else {
        bytes_out(bs)
    }
}

fn strs_out(ss: &[Vec<u8>]) -> Value {
    if ss.is_empty() {
        Value::Null
    } else {
        Value::Array(ss.iter().map(|s| gs(s)).collect())
    }
}

fn nil_hex(b: Option<&[u8]>) -> Value {
    b.map(|b| json!(hex(b))).unwrap_or(Value::Null)
}

/// `tcMapFail`.
fn map_fail(v: Value) -> Value {
    if v.get("panic") == Some(&json!(true)) || !v.get("err").map(|e| e.is_null()).unwrap_or(true) {
        json!({ "fail": true })
    } else {
        v
    }
}

/// `tcChecksumErr`: the runtime text of a recovered type-assertion panic is cut off.
fn checksum_err(e: &Error) -> Value {
    let msg = String::from_utf8_lossy(&e.error()).into_owned();
    match msg.find(": interface conversion") {
        Some(i) => json!(format!("{}: interface conversion", &msg[..i])),
        None => gs(&e.error()),
    }
}

// ---------------------------------------------------------------- exec

type Got = (Value, Option<Error>);

fn ok(v: Value) -> Got {
    (v, None)
}

fn exec(op: &str, a: &[Value]) -> Got {
    collate::set_new_collation_enabled_for_test(true);
    match op {
        "keys" => {
            let tid = a[0].as_i64().unwrap();
            let idx = a[1].as_i64().unwrap();
            let h = handle_opt(&a[2]);
            let enc = hexv(&a[3]);
            let (hs, he) = tc::get_table_handle_key_range(tid);
            let (is, ie) = tc::get_table_index_key_range(tid, idx);
            let mut o = json!({
                "tp": hex(tc::table_prefix()), "rp": hex(&tc::gen_table_record_prefix(tid).0),
                "ip": hex(&tc::gen_table_index_prefix(tid).0), "tbp": hex(&tc::gen_table_prefix(tid).0),
                "etp": hex(&tc::encode_table_prefix(tid).0), "eip": hex(&tc::encode_table_index_prefix(tid, idx).0),
                "seek": hex(&tc::encode_index_seek_key(tid, idx, &enc).0), "row": hex(&tc::encode_row_key(tid, &enc).0),
                "hr": [hex(&hs), hex(&he)], "ir": [hex(&is), hex(&ie)],
            });
            if let Some(h) = &h {
                o["rowh"] = json!(hex(&tc::encode_row_key_with_handle(tid, h).0));
                o["rec"] = json!(hex(&tc::encode_record_key(
                    &tc::gen_table_record_prefix(tid).0,
                    h
                )
                .0));
            }
            ok(o)
        }
        "key_decode" => {
            let k = hexv(&a[0]);
            let k = &k[..];
            let mut o = serde_json::Map::new();
            o.insert(
                "rec".into(),
                try_(|| match tc::decode_record_key(k) {
                    Ok((tid, h)) => json!({ "tid": tid, "h": handle_out(&h), "err": null }),
                    Err(e) => json!({ "tid": 0, "h": null, "err": gs(&e.error()) }),
                }),
            );
            o.insert(
                "row".into(),
                try_(|| match tc::decode_row_key(k) {
                    Ok(h) => json!({ "h": handle_out(&h) }),
                    Err(e) => json!({ "err": gs(&e.error()) }),
                }),
            );
            o.insert(
                "head".into(),
                try_(|| {
                    let (tid, iid, rec, err) = tc::decode_key_head(k);
                    json!({ "tid": tid, "iid": iid, "rec": rec, "err": err_msg(&err) })
                }),
            );
            o.insert(
                "idx".into(),
                try_(|| match tc::decode_index_key(k) {
                    Ok((tid, iid, vals)) => {
                        json!({ "tid": tid, "iid": iid, "vals": strs_out(&vals), "err": null })
                    }
                    Err(e) => json!({ "tid": 0, "iid": 0, "vals": null, "err": gs(&e.error()) }),
                }),
            );
            o.insert(
                "vals".into(),
                try_(|| match tc::decode_values_bytes_to_strings(k) {
                    Ok(vals) => json!({ "vals": strs_out(&vals), "err": null }),
                    Err(e) => json!({ "vals": null, "err": gs(&e.error()) }),
                }),
            );
            o.insert("tid".into(), try_(|| json!(tc::decode_table_id(k))));
            o.insert("tid_func".into(), try_(|| json!((tc::init())(k))));
            o.insert(
                "meta".into(),
                try_(|| match tc::decode_meta_key(k) {
                    Ok((key, field)) => json!({ "key": hex(&key), "field": hex(&field) }),
                    Err(e) => json!({ "err": gs(&e.error()) }),
                }),
            );
            o.insert(
                "is".into(),
                try_(|| {
                    json!([
                        tc::is_record_key(k),
                        tc::is_index_key(k),
                        tc::is_table_key(k)
                    ])
                }),
            );
            o.insert("kind".into(), json!(get_key_kind(k) as u8));
            o.insert("temp".into(), try_(|| json!(tc::is_temp_index_key(k))));
            o.insert(
                "to_temp".into(),
                try_(|| {
                    let mut c = k.to_vec();
                    tc::index_key2_temp_index_key(&mut c);
                    json!(hex(&c))
                }),
            );
            ok(Value::Object(o))
        }
        "encode_value" => {
            let loc = loc_opt(&a[0]);
            let d = datum_in(&a[2]);
            let (b, err) = tc::encode_value_datum(loc.as_ref(), hexv(&a[1]), &d);
            // Go returns a nil slice on a flatten error.
            let nil = err.is_err() && b.is_empty();
            (json!({ "b": hex(&b), "nil": nil }), err.err())
        }
        "unflatten" => {
            let d = datum_in(&a[0]);
            let ft = ft_in(&a[1]);
            let loc = loc_opt(&a[2]);
            let data = hexv(&a[3]);
            let u = try_(|| {
                let (r, err) = tc::unflatten(d.clone(), &ft, loc.as_ref());
                json!({ "d": datum_out(&r), "err": err_msg(&err) })
            });
            let dcv = try_(|| match tc::decode_column_value(&data, &ft, loc.as_ref()) {
                Ok(r) => json!({ "d": datum_out(&r), "err": null }),
                Err(e) => json!({ "d": datum_out(&Datum::default()), "err": gs(&e.error()) }),
            });
            let dcvw = try_(|| {
                let mut r = Datum::default();
                let err = tc::decode_column_value_with_datum(&data, &ft, loc.as_ref(), &mut r);
                json!({ "d": datum_out(&r), "err": err_msg(&err) })
            });
            ok(json!({ "u": u, "dcv": dcv, "dcvw": dcvw }))
        }
        "row" => ok(row(a)),
        "checksum" => {
            let cols = a[0].as_array().unwrap();
            let ds = datums_arg(&a[1]);
            let loc = loc_opt(&a[2]);
            let infos: Vec<ColumnInfo> = cols
                .iter()
                .map(|q| ColumnInfo {
                    id: q[0].as_i64().unwrap(),
                    field_type: ft_in(&q[1]),
                    ..ColumnInfo::default()
                })
                .collect();
            let mut rd = RowData::default();
            let mut encs = vec![];
            for i in 0..infos.len() {
                let cd = ColData {
                    column_info: &infos[i],
                    datum: &ds[i],
                };
                rd.cols.push(cd);
                encs.push(try_(|| match cd.encode(loc.as_ref(), Vec::new()) {
                    Ok(b) => json!({ "b": hex(&b), "err": null }),
                    Err(e) => json!({ "b": "", "err": checksum_err(&e) }),
                }));
            }
            // sort.Sort(rd): Go's sort over Len/Less/Swap (insertion sort below 12 elements).
            for i in 1..rd.len() {
                let mut j = i;
                while j > 0 && rd.less(j, j - 1) {
                    rd.swap(j, j - 1);
                    j -= 1;
                }
            }
            let sum = try_(|| match rd.checksum(loc.as_ref()) {
                Ok(c) => json!({ "sum": c, "err": null }),
                Err(e) => json!({ "sum": 0, "err": checksum_err(&e) }),
            });
            ok(json!({ "encs": encs, "sum": sum }))
        }
        "index" => ok(index(a)),
        "encode_row" => {
            // [ids, datums, loc, checksum handle spec or null]
            let ids: Vec<i64> = a[0]
                .as_array()
                .map(|v| v.iter().map(|v| v.as_i64().unwrap()).collect())
                .unwrap_or_default();
            let ds = if a[1].is_null() {
                Vec::new()
            } else {
                datums_arg(&a[1])
            };
            let loc = loc_opt(&a[2]);
            let cs = handle_opt(&a[3]).map(|handle| rowcodec::Checksum::Raw { handle });
            ok(try_(|| {
                let mut enc = rowcodec::Encoder::with_enable(true);
                match enc.encode(loc.as_ref(), &ids, &ds, cs.as_ref(), Vec::new()) {
                    Ok(b) => json!({ "b": hex(&b), "err": null }),
                    Err(e) => json!({ "b": "", "err": gs(&e.error()) }),
                }
            }))
        }
        "index_value" => ok(index_value(a)),
        "index_raw" => {
            let k = hexv(&a[0]);
            let v = hexv(&a[1]);
            ok(index_decode(
                &k,
                &v,
                a[2].as_u64().unwrap() as usize,
                &col_infos(&a[3]),
                a[4].as_bool().unwrap(),
            ))
        }
        "truncate" => {
            let mut d = datum_in(&a[0]);
            let idx_col = IndexColumn {
                length: a[1].as_i64().unwrap() as isize,
                ..IndexColumn::default()
            };
            let mut col = ColumnInfo::default();
            col.field_type.set_charset(a[2].as_str().unwrap());
            col.field_type.set_collate(a[3].as_str().unwrap());
            tc::truncate_index_value(&mut d, &idx_col, &col);
            ok(datum_out(&d))
        }
        "verify_ranges" => {
            let parts: Vec<Vec<KeyRange>> = a[0]
                .as_array()
                .map(|x| x.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|part| {
                    part.as_array()
                        .map(|x| x.as_slice())
                        .unwrap_or(&[])
                        .iter()
                        .map(|r| KeyRange {
                            start_key: tidb_kv::key::Key(unhex(r[0].as_str().unwrap())),
                            end_key: tidb_kv::key::Key(unhex(r[1].as_str().unwrap())),
                        })
                        .collect()
                })
                .collect();
            let krs = if a[1].as_bool().unwrap() {
                new_partitioned_key_ranges(parts)
            } else {
                new_non_partitioned_key_ranges(parts.into_iter().flatten().collect())
            };
            let (tids, err) = tc::verify_table_id_for_ranges(&krs);
            let tids = if tids.is_empty() {
                Value::Array(vec![])
            } else {
                json!(tids)
            };
            (json!({ "tids": tids }), err.err())
        }
        _ => panic!("unknown op {op}"),
    }
}

fn row(a: &[Value]) -> Value {
    let b = hexv(&a[0]);
    let cols = cols_map(&a[1]);
    let loc = loc_opt(&a[2]);
    let h = handle_opt(&a[3]);
    let hids: Vec<i64> = a[4]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect();
    let infos = col_infos(&a[5]);
    let commit_ts = a[6].as_u64().unwrap();
    let mut o = serde_json::Map::new();
    o.insert(
        "map".into(),
        map_fail(try_(|| {
            match tc::decode_row_to_datum_map(&b, &cols, loc.as_ref()) {
                Ok(m) => json!({ "m": map_out(&m), "err": null }),
                Err(e) => json!({ "m": null, "err": gs(&e.error()) }),
            }
        })),
    );
    o.insert(
        "hmap".into(),
        try_(|| {
            let (m, err) =
                tc::decode_handle_to_datum_map(h.as_ref(), &hids, &cols, loc.as_ref(), None);
            // Go returns a nil map when there is no handle.
            let mo = if h.is_none() || hids.is_empty() {
                Value::Null
            } else {
                map_out(&m)
            };
            json!({ "m": mo, "err": err_msg(&err) })
        }),
    );
    let new_fmt = !b.is_empty() && rowcodec::is_new_format(&b);
    if !new_fmt {
        o.insert(
            "v1".into(),
            try_(
                || match tc::decode_row_with_map(&b, &cols, loc.as_ref(), None) {
                    Ok(m) => json!({ "m": map_out(&m), "err": null }),
                    Err(e) => json!({ "m": null, "err": gs(&e.error()) }),
                },
            ),
        );
        let mut col_ids = HashMap::new();
        for (i, ci) in infos.iter().enumerate() {
            col_ids.insert(ci.id, i);
        }
        // Go returns a nil slice for the one-byte NilFlag row only.
        let nil_row = b.len() == 1 && b[0] == 0;
        o.insert(
            "cut".into(),
            try_(|| match tc::cut_row_new(&b, &col_ids) {
                Ok(r) => {
                    json!({ "r": if nil_row { Value::Null } else { bytes_out(&r) }, "err": null })
                }
                Err(e) => json!({ "r": null, "err": gs(&e.error()) }),
            }),
        );
        return Value::Object(o);
    }
    o.insert(
        "v2".into(),
        map_fail(try_(|| {
            match tc::decode_row_with_map_new(&b, &cols, loc.as_ref(), None) {
                Ok(m) => json!({ "m": map_out(&m), "err": null }),
                Err(e) => json!({ "m": null, "err": gs(&e.error()) }),
            }
        })),
    );
    o.insert(
        "dm".into(),
        try_(|| {
            let dec = rowcodec::new_datum_map_decoder(infos.clone(), loc.clone());
            let res = dec.decode_to_datum_map(&b, None);
            // Go's decoder keeps the row it parsed (its embedded row), also after an error.
            let mut r = Row::default();
            let _ = r.from_bytes(&b);
            let (c1, ok1) = r.get_checksum();
            let (c2, ok2) = r.get_extra_checksum();
            let cs = json!([c1, ok1, c2, ok2, r.checksum_version()]);
            match res {
                Ok(m) => json!({ "m": map_out(&m), "err": null, "cs": cs }),
                Err(e) => json!({ "m": null, "err": gs(&e.error()), "cs": cs }),
            }
        }),
    );
    let defs: Option<Vec<Value>> = if a[7].is_null() {
        None
    } else {
        Some(a[7].as_array().unwrap().clone())
    };
    let fts: Vec<FieldType> = infos.iter().map(|c| (*c.ft).clone()).collect();
    o.insert(
        "chk".into(),
        try_(|| {
            let def: Option<rowcodec::DefDatumFn> = defs.clone().map(|defs| {
                Box::new(
                    move |i: usize, chk: &mut tidb_chunk::Chunk| -> Result<(), Error> {
                        if defs[i].is_null() {
                            chk.append_null(i);
                        } else {
                            chk.append_datum(i, &datum_in(&defs[i]));
                        }
                        Ok(())
                    },
                ) as rowcodec::DefDatumFn
            });
            let mut chk = new_chunk_with_capacity(&fts, 1);
            let dec = rowcodec::new_chunk_decoder(infos.clone(), hids.clone(), def, loc.clone());
            if let Err(e) = dec.decode_to_chunk(&b, commit_ts, h.as_ref(), &mut chk) {
                return json!({ "err": gs(&e.error()) });
            }
            let mut cells = vec![];
            for (c, ft) in fts.iter().enumerate() {
                cells.push(try_(|| {
                    if chk.num_rows() == 0 || chk.column(c).rows() == 0 {
                        return json!("none");
                    }
                    if chk.column(c).is_null(0) {
                        return Value::Null;
                    }
                    datum_out(&chk.get_row(0).get_datum(c, ft))
                }));
            }
            json!({ "cells": if cells.is_empty() { Value::Null } else { Value::Array(cells) } })
        }),
    );
    let def_bytes: Option<Vec<Value>> = if a[8].is_null() {
        None
    } else {
        Some(a[8].as_array().unwrap().clone())
    };
    let cache = hexv(&a[9]);
    let mut offs = HashMap::new();
    for (i, ci) in infos.iter().enumerate() {
        offs.insert(ci.id, i);
    }
    let mk = || {
        let def: Option<rowcodec::DefBytesFn> = def_bytes.clone().map(|d| {
            Box::new(move |i: usize| -> Result<Vec<u8>, Error> {
                Ok(d[i].as_str().map(unhex).unwrap_or_default())
            }) as rowcodec::DefBytesFn
        });
        rowcodec::new_byte_decoder(infos.clone(), hids.clone(), def, loc.clone())
    };
    o.insert(
        "bytes".into(),
        try_(
            || match mk().decode_to_bytes(&offs, h.as_ref(), &b, &cache) {
                Ok(r) => json!({ "r": bytes_out(&r), "err": null }),
                Err(e) => json!({ "r": null, "err": gs(&e.error()) }),
            },
        ),
    );
    o.insert(
        "bytes_nh".into(),
        try_(|| match mk().decode_to_bytes_no_handle(&offs, &b) {
            Ok(r) => json!({ "r": bytes_out(&r), "err": null }),
            Err(e) => json!({ "r": null, "err": gs(&e.error()) }),
        }),
    );
    Value::Object(o)
}

fn index(a: &[Value]) -> Value {
    let mut tbl = TableInfo::default();
    tbl.unmarshal_json(a[0].as_str().unwrap().as_bytes())
        .unwrap();
    let idx_off = a[1].as_u64().unwrap() as usize;
    let phy = a[2].as_i64().unwrap();
    let ds = datums_arg(&a[3]);
    let h = handle_opt(&a[4]);
    let value = hexv(&a[5]);
    let cols_len = a[6].as_u64().unwrap() as usize;
    let cols = col_infos(&a[7]);
    let loc = loc_arg(&a[8]);
    let idx = &tbl.indices.as_ref().unwrap()[idx_off];
    let mut key: Option<Vec<u8>> = None;
    let gen_out = try_(|| {
        let mut vals = ds.clone();
        match tc::gen_index_key(&loc, &tbl, idx, phy, &mut vals, h.as_ref(), None) {
            Ok((k, distinct)) => {
                let o = json!({ "k": hex(&k), "nil": false, "distinct": distinct, "err": null, "vals": datum_outs(&vals) });
                key = Some(k);
                o
            }
            Err(e) => {
                json!({ "k": "", "nil": true, "distinct": false, "err": gs(&e.error()), "vals": datum_outs(&vals) })
            }
        }
    });
    let mut o = serde_json::Map::new();
    o.insert("gen".into(), gen_out);
    if let Some(k) = key {
        o.insert(
            "dec".into(),
            index_decode(&k, &value, cols_len, &cols, idx.unique),
        );
    }
    Value::Object(o)
}

/// `index_value`: [table JSON, index offset, physical ID, row datums, handle spec, untouched,
/// loc, needRestored] → GenIndexKey + TruncateIndexValues + GenIndexValuePortal (no handle
/// restored data: the generator only emits rows where it is empty).
fn index_value(a: &[Value]) -> Value {
    let mut tbl = TableInfo::default();
    tbl.unmarshal_json(a[0].as_str().unwrap().as_bytes())
        .unwrap();
    let idx_off = a[1].as_u64().unwrap() as usize;
    let phy = a[2].as_i64().unwrap();
    let row = datums_arg(&a[3]);
    let h = handle_opt(&a[4]);
    let untouched = a[5].as_bool().unwrap();
    let loc = loc_arg(&a[6]);
    let need_restored = a[7].as_bool().unwrap();
    let idx = &tbl.indices.as_ref().unwrap()[idx_off];
    try_(|| {
        let mut vals: Vec<Datum> = idx
            .columns
            .as_ref()
            .unwrap()
            .iter()
            .map(|ic| row[ic.offset as usize].clone())
            .collect();
        let mut key_vals = vals.clone();
        let (k, distinct) =
            match tc::gen_index_key(&loc, &tbl, idx, phy, &mut key_vals, h.as_ref(), None) {
                Ok(r) => r,
                Err(e) => return json!({ "err": gs(&e.error()) }),
            };
        tc::truncate_index_values(&tbl, idx, &mut vals);
        let pid = if idx.global { phy } else { 0 };
        let hh = h.clone().expect("index_value: handle");
        match tc::gen_index_value_portal(
            Some(&loc),
            &tbl,
            idx,
            need_restored,
            distinct,
            untouched,
            &vals,
            &hh,
            pid,
            &[],
            None,
        ) {
            Ok(v) => json!({ "k": hex(&k), "distinct": distinct, "v": hex(&v), "err": null }),
            Err(e) => json!({ "k": hex(&k), "distinct": distinct, "v": "", "err": gs(&e.error()) }),
        }
    })
}

fn index_decode(
    key: &[u8],
    value: &[u8],
    cols_len: usize,
    cols: &[ColInfo],
    unique: bool,
) -> Value {
    let mut o = serde_json::Map::new();
    for (i, hs) in [
        tc::HandleStatus::HandleDefault,
        tc::HandleStatus::HandleIsUnsigned,
        tc::HandleStatus::HandleNotNeeded,
    ]
    .into_iter()
    .enumerate()
    {
        let c = if hs == tc::HandleStatus::HandleNotNeeded && cols.len() > cols_len {
            &cols[..cols_len]
        } else {
            cols
        };
        o.insert(
            format!("kv{i}"),
            try_(|| match tc::decode_index_kv(key, value, cols_len, hs, c) {
                Ok(r) => json!({ "r": bytes_out(&r), "err": null }),
                Err(e) => json!({ "r": null, "err": gs(&e.error()) }),
            }),
        );
    }
    o.insert(
        "kvex".into(),
        try_(|| {
            let pre = vec![Cow::Borrowed(&[][..]); cols_len];
            match tc::decode_index_kv_ex(
                key,
                value,
                cols_len,
                tc::HandleStatus::HandleDefault,
                cols,
                &[0xaa],
                pre,
            ) {
                Ok(r) => json!({ "r": bytes_out(&r), "err": null }),
                Err(e) => json!({ "r": null, "err": gs(&e.error()) }),
            }
        }),
    );
    o.insert(
        "h".into(),
        try_(|| match tc::decode_index_handle(key, value, cols_len) {
            Ok(h) => json!({ "h": h.as_ref().map(handle_out).unwrap_or(Value::Null), "err": null }),
            Err(e) => json!({ "h": null, "err": gs(&e.error()) }),
        }),
    );
    if unique {
        o.insert(
            "hv".into(),
            try_(|| match tc::decode_handle_in_index_value(value) {
                Ok(h) => {
                    json!({ "h": h.as_ref().map(handle_out).unwrap_or(Value::Null), "err": null })
                }
                Err(e) => json!({ "h": null, "err": gs(&e.error()) }),
            }),
        );
    }
    o.insert(
        "seg".into(),
        try_(|| {
            let s = tc::split_index_value(value);
            json!([
                nil_hex(s.common_handle),
                nil_hex(s.partition_id),
                nil_hex(s.restored_values),
                nil_hex(s.int_handle)
            ])
        }),
    );
    o.insert(
        "cut".into(),
        try_(|| {
            let (vs, rest, err) = tc::cut_index_key_new(key, cols_len);
            json!({ "vs": bytes_out_or_null(&vs), "rest": hex(rest), "err": err_msg(&err) })
        }),
    );
    Value::Object(o)
}

// ---------------------------------------------------------------- replay

/// Equality with `{"fail": true}` matching itself only and `{"panic": true}` matching a
/// Rust panic (both already mapped by try_).
fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
    collate::set_new_collation_enabled_for_test(true);
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
    if out != *want_out {
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
    if got_err != want_err_cmp {
        return Some(format!("err: got {got_err:?}, want {want_err_cmp:?}"));
    }
    None
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "tablecodec");
    tidb_codec::init();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
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
fn govec_tablecodec() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_TABLECODEC_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
