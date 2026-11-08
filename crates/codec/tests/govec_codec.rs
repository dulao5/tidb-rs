//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/codec.jsonl.gz` (tracker
//! `govec`, domain `codec`: block D01.8, pkg/util/codec) against the Rust port.
//!
//! Every record's `out` must be equal as JSON; errors must match by code and message;
//! `err.name == "panic"` records, and `{"panic": true}` sub-results, must panic in Rust too.
//!
//! Extra vector files (other seeds, e.g. tracker CI's `govec codec -seed N`) are replayed
//! too when `GOVEC_CODEC_FILES` lists them (`:`-separated paths).

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::gostd::time::{Location, fixed_zone, utc};
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_codec as codec;
use tidb_parser::types::{FieldType, IHasher};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/codec.jsonl.gz",
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

fn fts_arg(v: &Value) -> Vec<FieldType> {
    v.as_array().unwrap().iter().map(ft_in).collect()
}

fn chunk_arg(a: &[Value], i: usize) -> (Vec<FieldType>, tidb_chunk::Chunk) {
    let fts = fts_arg(&a[i]);
    let rows = a[i + 1].as_array().unwrap();
    let mut chk = new_chunk_with_capacity(&fts, rows.len());
    for r in rows {
        for (c, q) in r.as_array().unwrap().iter().enumerate() {
            chk.append_datum(c, &datum_in(q));
        }
    }
    (fts, chk)
}

fn bools_opt(v: &Value) -> Option<Vec<bool>> {
    if v.is_null() {
        None
    } else {
        Some(
            v.as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_bool().unwrap())
                .collect(),
        )
    }
}

fn usizes(v: &Value) -> Vec<usize> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_u64().unwrap() as usize)
        .collect()
}

/// Go `var s []T` marshals as `null` when nothing was appended.
fn list_or_null(v: Vec<Value>) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        Value::Array(v)
    }
}

/// `codecTry`: a panic is `{"panic": true}`.
fn try_(f: impl FnOnce() -> Value) -> Value {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => json!({ "panic": true }),
    }
}

/// hash.Hash64 recorder.
#[derive(Default)]
struct RecHash(Vec<u8>);

impl std::hash::Hasher for RecHash {
    fn finish(&self) -> u64 {
        0
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

/// base.Hasher recorder.
#[derive(Default)]
struct RecHasher(Vec<u8>);

impl IHasher for RecHasher {
    fn hash_bool(&mut self, _: bool) {
        unreachable!()
    }
    fn hash_int(&mut self, _: isize) {
        unreachable!()
    }
    fn hash_int64(&mut self, _: i64) {
        unreachable!()
    }
    fn hash_uint64(&mut self, _: u64) {
        unreachable!()
    }
    fn hash_float64(&mut self, _: f64) {
        unreachable!()
    }
    fn hash_rune(&mut self, _: char) {
        unreachable!()
    }
    fn hash_string(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn hash_byte(&mut self, _: u8) {
        unreachable!()
    }
    fn hash_bytes(&mut self, val: &[u8]) {
        self.0.extend_from_slice(val);
    }
    fn reset(&mut self) {
        self.0.clear();
    }
    fn sum64(&self) -> u64 {
        0
    }
}

type Got = (Value, Option<Error>);

fn ok(v: Value) -> Got {
    (v, None)
}

fn with(v: Value, r: Result<(), Error>) -> Got {
    (v, r.err())
}

fn rem_out<T: Into<Value>>(r: Result<(&[u8], T), Error>, zero: Value) -> Got {
    match r {
        Ok((rem, v)) => ok(json!({ "rem": hex(rem), "v": v.into() })),
        Err(e) => (json!({ "rem": "", "v": zero }), Some(e)),
    }
}

fn dec(fn_: &str, b: &[u8]) -> Got {
    match fn_ {
        "int" => rem_out(codec::decode_int(b), json!(0)),
        "int_desc" => rem_out(codec::decode_int_desc(b), json!(0)),
        "uint" => rem_out(codec::decode_uint(b), json!(0)),
        "uint_desc" => rem_out(codec::decode_uint_desc(b), json!(0)),
        "varint" => rem_out(codec::decode_varint(b), json!(0)),
        "uvarint" => rem_out(codec::decode_uvarint(b), json!(0)),
        "cmp_varint" => rem_out(codec::decode_comparable_varint(b), json!(0)),
        "cmp_uvarint" => rem_out(codec::decode_comparable_uvarint(b), json!(0)),
        // decodeCmpUintToFloat(0) with the error: the bits of ^0.
        "float" => rem_out(
            codec::decode_float(b).map(|(r, f)| (r, f.to_bits())),
            json!(u64::MAX),
        ),
        "float_desc" => rem_out(
            codec::decode_float_desc(b).map(|(r, f)| (r, f.to_bits())),
            json!(u64::MAX),
        ),
        "bytes" => rem_out(
            codec::decode_bytes(b, None).map(|(r, v)| (r, hex(&v))),
            json!(""),
        ),
        "bytes_desc" => rem_out(
            codec::decode_bytes_desc(b, None).map(|(r, v)| (r, hex(&v))),
            json!(""),
        ),
        "compact_bytes" => rem_out(
            codec::decode_compact_bytes(b).map(|(r, v)| (r, hex(v))),
            json!(""),
        ),
        "decimal" => {
            let (rem, d, prec, frac, err) = codec::decode_decimal(b);
            with(
                json!({ "rem": hex(rem), "v": {
                    "dec": d.as_ref().map(dec_out).unwrap_or(Value::Null), "prec": prec, "frac": frac,
                }}),
                err,
            )
        }
        _ => panic!("dec {fn_}"),
    }
}

fn exec(op: &str, a: &[Value]) -> Got {
    collate::set_new_collation_enabled_for_test(true);
    let ctx = &*DefaultStmtNoWarningContext;
    match op {
        "enc_int" => {
            let v = a[0].as_i64().unwrap();
            let f = |g: fn(&mut Vec<u8>, i64)| {
                let mut b = Vec::new();
                g(&mut b, v);
                hex(&b)
            };
            ok(json!({
                "asc": f(codec::encode_int), "desc": f(codec::encode_int_desc),
                "var": f(codec::encode_varint), "cmpvar": f(codec::encode_comparable_varint),
                "cmp": codec::encode_int_to_cmp_uint(v), "back": codec::decode_cmp_uint_to_int(v as u64),
            }))
        }
        "enc_uint" => {
            let v = a[0].as_u64().unwrap();
            let f = |g: fn(&mut Vec<u8>, u64)| {
                let mut b = Vec::new();
                g(&mut b, v);
                hex(&b)
            };
            ok(json!({
                "asc": f(codec::encode_uint), "desc": f(codec::encode_uint_desc),
                "var": f(codec::encode_uvarint), "cmpvar": f(codec::encode_comparable_uvarint),
            }))
        }
        "enc_float" => {
            let v = bits_f(&a[0]);
            let f = |g: fn(&mut Vec<u8>, f64)| {
                let mut b = Vec::new();
                g(&mut b, v);
                hex(&b)
            };
            ok(json!({ "asc": f(codec::encode_float), "desc": f(codec::encode_float_desc) }))
        }
        "enc_bytes" => {
            let (pre, data) = (hexv(&a[0]), hexv(&a[1]));
            let f = |g: &dyn Fn(&mut Vec<u8>)| {
                let mut b = pre.clone();
                g(&mut b);
                hex(&b)
            };
            ok(json!({
                "asc": f(&|b| codec::encode_bytes(b, &data)),
                "desc": f(&|b| codec::encode_bytes_desc(b, &data)),
                "compact": f(&|b| codec::encode_compact_bytes(b, &data)),
                "len": codec::encoded_bytes_length(data.len() as isize),
                "ext": f(&|b| codec::encode_bytes_ext(b, &data, true)),
                "ext2": f(&|b| codec::encode_bytes_ext(b, &data, false)),
            }))
        }
        "dec" => dec(a[0].as_str().unwrap(), &hexv(&a[1])),
        "enc_decimal" => {
            let d = dec_of(a[0].as_str().unwrap());
            let (prec, frac) = (
                a[1].as_i64().unwrap() as isize,
                a[2].as_i64().unwrap() as isize,
            );
            let mut b = Vec::new();
            let err = codec::encode_decimal(&mut b, &d, prec, frac);
            let mut dd = Datum::default();
            dd.set_mysql_decimal(d);
            dd.set_length(prec);
            dd.set_frac(frac);
            let (size, size_err) = match codec::estimate_value_size(ctx, &dd) {
                Ok(s) => (s, Value::Null),
                Err(e) => (0, gs(&e.error())),
            };
            with(
                json!({ "b": hex(&b), "size": size, "size_err": size_err }),
                err,
            )
        }
        "encode" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let loc = loc_arg(&a[2]);
            let mut b = hexv(&a[3]);
            let ds = datums_arg(&a[4]);
            let err = if a[1].as_bool().unwrap() {
                codec::encode_key(&loc, &mut b, &ds)
            } else {
                codec::encode_value(&loc, &mut b, &ds)
            };
            // Go returns a nil slice only from the EncodeMySQLTime error.
            let is_nil = b.is_empty() && err.is_err() && b.capacity() == 0;
            with(json!({ "b": hex(&b), "nil": is_nil }), err)
        }
        "mysql_time" => {
            let mut b = hexv(&a[3]);
            let err = codec::encode_mysql_time(
                &loc_arg(&a[2]),
                Time::from_raw(CoreTime(a[0].as_u64().unwrap())),
                a[1].as_u64().unwrap() as u8,
                &mut b,
            );
            let is_nil = err.is_err();
            with(json!({ "b": hex(&b), "nil": is_nil }), err)
        }
        "estimate" => match codec::estimate_value_size(ctx, &datum_in(&a[0])) {
            Ok(s) => ok(json!(s)),
            Err(e) => (json!(0), Some(e)),
        },
        "hash_code" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let mut b = hexv(&a[2]);
            codec::hash_code(&mut b, &datum_in(&a[1]));
            ok(json!(hex(&b)))
        }
        "hash64" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let mut h = RecHasher::default();
            codec::hash64(&mut h, &datum_in(&a[1]));
            ok(json!(hex(&h.0)))
        }
        "decode_one" => {
            let b = hexv(&a[0]);
            let one = try_(|| {
                let (rem, d, err) = codec::decode_one(&b);
                json!({ "rem": hex(rem), "d": datum_out(&d), "err": err_msg(&err) })
            });
            let cut = try_(|| match codec::cut_one(&b) {
                Ok((data, rem)) => json!({ "data": hex(data), "rem": hex(rem), "err": null }),
                Err(e) => json!({ "data": "", "rem": "", "err": gs(&e.error()) }),
            });
            let all = try_(|| match codec::decode(&b, 2) {
                Ok(ds) => json!({ "ds": datum_outs(&ds), "err": null }),
                Err(e) => json!({ "ds": [], "err": gs(&e.error()) }),
            });
            let cid = try_(|| match codec::cut_column_id(&b) {
                Ok((rem, id)) => json!({ "rem": hex(rem), "id": id, "err": null }),
                Err(e) => json!({ "rem": "", "id": 0, "err": gs(&e.error()) }),
            });
            let raw = try_(|| {
                let mut raws = vec![Datum::default(), Datum::default()];
                let err = codec::set_raw_values(&b, &mut raws);
                json!({ "ds": datum_outs(&raws), "err": err_msg(&err) })
            });
            ok(json!({ "one": one, "cut": cut, "all": all, "cid": cid, "raw": raw }))
        }
        "decode_range" => {
            let b = hexv(&a[0]);
            let tps = hex_opt(&a[2]);
            let loc = loc_opt(&a[3]);
            let (vals, rem, err) = codec::decode_range(
                &b,
                a[1].as_u64().unwrap() as usize,
                tps.as_deref(),
                loc.as_ref(),
            );
            with(json!({ "vals": datum_outs(&vals), "rem": hex(rem) }), err)
        }
        "decode_as" => {
            let b = hexv(&a[0]);
            let tp = a[1].as_u64().unwrap() as u8;
            let (rem, d, err) = if a[2].as_bool().unwrap() {
                codec::decode_as_float32(&b, tp)
            } else {
                let loc = loc_opt(&a[3]);
                codec::decode_as_date_time(&b, tp, loc.as_ref())
            };
            with(json!({ "rem": hex(rem), "d": datum_out(&d) }), err)
        }
        "chunk_hash" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let (fts, chk) = chunk_arg(a, 1);
            let col_idx = usizes(&a[3]);
            let tps: Vec<FieldType> = col_idx.iter().map(|&c| fts[c].clone()).collect();
            let mut rows = vec![];
            let mut eq = vec![];
            let n = chk.num_rows();
            for i in 0..n {
                let mut w: Vec<u8> = Vec::new();
                let mut buf = [0u8; 1];
                if let Err(e) =
                    codec::hash_chunk_row(ctx, &mut w, chk.get_row(i), &tps, &col_idx, &mut buf)
                {
                    return (list_or_null(rows), Some(e));
                }
                rows.push(json!(hex(&w)));
                let j = (i + 1) % n;
                let r = codec::equal_chunk_row(
                    ctx,
                    chk.get_row(i),
                    &tps,
                    &col_idx,
                    chk.get_row(j),
                    &tps,
                    &col_idx,
                );
                eq.push(match r {
                    Ok(e) => json!({ "eq": e, "err": null }),
                    Err(e) => json!({ "eq": false, "err": gs(&e.error()) }),
                });
            }
            let merr = codec::equal_chunk_row(
                ctx,
                chk.get_row(0),
                &tps,
                &col_idx,
                chk.get_row(0),
                &tps,
                &col_idx[..0],
            );
            let merr = match merr {
                Ok(_) => Value::Null,
                Err(e) => gs(&e.error()),
            };
            ok(json!({ "rows": list_or_null(rows), "eq": list_or_null(eq), "mismatch": merr }))
        }
        "chunk_selected" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let (fts, chk) = chunk_arg(a, 1);
            let col = a[3].as_u64().unwrap() as usize;
            let sel = bools_opt(&a[4]);
            let ignore_null = a[5].as_bool().unwrap();
            let n = chk.num_rows();
            let mut hs: Vec<RecHash> = (0..n).map(|_| RecHash::default()).collect();
            let mut is_null = vec![false; n];
            let mut buf = [0u8; 1];
            let err = if sel.is_none() && !ignore_null {
                codec::hash_chunk_columns(
                    ctx,
                    &mut hs,
                    &chk,
                    &fts[col],
                    col,
                    &mut buf,
                    &mut is_null,
                )
            } else {
                codec::hash_chunk_selected(
                    ctx,
                    &mut hs,
                    &chk,
                    &fts[col],
                    col,
                    &mut buf,
                    &mut is_null,
                    sel.as_deref(),
                    ignore_null,
                )
            };
            let rows: Vec<Value> = hs.iter().map(|h| json!(hex(&h.0))).collect();
            with(json!({ "rows": list_or_null(rows), "null": is_null }), err)
        }
        "serialize_keys" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let (fts, chk) = chunk_arg(a, 1);
            let col = a[3].as_u64().unwrap() as usize;
            let used = usizes(&a[4]);
            let filter = bools_opt(&a[5]);
            let mut nullv = a[6].as_bool().unwrap().then(|| vec![false; chk.num_rows()]);
            let mode = codec::SerializeMode(a[7].as_i64().unwrap() as isize);
            let mut keys = vec![Vec::new(); used.len()];
            let err = codec::serialize_keys(
                ctx,
                &chk,
                &fts[col],
                col,
                &used,
                filter.as_deref(),
                nullv.as_deref_mut(),
                mode,
                &mut keys,
            );
            let ks: Vec<Value> = keys.iter().map(|k| json!(hex(k))).collect();
            with(
                json!({ "keys": list_or_null(ks), "null": nullv.map(|v| json!(v)).unwrap_or(Value::Null) }),
                err,
            )
        }
        "hash_group_key" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let (fts, chk) = chunk_arg(a, 1);
            let loc = loc_arg(&a[3]);
            let n = chk.num_rows();
            let mut buf = vec![Vec::new(); n];
            let err = codec::hash_group_key(&loc, n, chk.column(0), &mut buf, &fts[0]);
            let bs: Vec<Value> = buf.iter().map(|b| json!(hex(b))).collect();
            with(json!({ "buf": list_or_null(bs), "nil": false }), err)
        }
        "decoder" => {
            let fts = fts_arg(&a[0]);
            let mut chk = new_chunk_with_capacity(&fts, 4);
            let mut rems = vec![];
            {
                let mut dec = codec::new_decoder(&mut chk, loc_arg(&a[2]));
                for r in a[1].as_array().unwrap() {
                    for (c, v) in r.as_array().unwrap().iter().enumerate() {
                        let b = unhex(v.as_str().unwrap());
                        rems.push(try_(|| match dec.decode_one(&b, c, &fts[c]) {
                            Ok(rem) => json!(hex(rem)),
                            Err(e) => json!({ "err": gs(&e.error()) }),
                        }));
                    }
                }
            }
            let mut cols = vec![];
            for (c, ft) in fts.iter().enumerate() {
                let mut cells = vec![];
                for i in 0..chk.column(c).rows() {
                    cells.push(try_(|| {
                        if chk.column(c).is_null(i) {
                            return Value::Null;
                        }
                        datum_out(&chk.get_row(i).get_datum(c, ft))
                    }));
                }
                cols.push(list_or_null(cells));
            }
            ok(json!({ "rems": list_or_null(rems), "cols": list_or_null(cols) }))
        }
        "convert_by_collation" => {
            collate::set_new_collation_enabled_for_test(a[0].as_bool().unwrap());
            let ft = ft_in(&a[1]);
            let s = ungs(&a[2]);
            ok(json!({
                "b": hex(&codec::convert_by_collation(&s, &ft)),
                "s": hex(&codec::convert_by_collation_str(&s, &ft)),
            }))
        }
        _ => panic!("unknown op {op}"),
    }
}

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
    // Go's value next to an error is compared only where the op records it explicitly.
    if out != *want_out && !(err.is_some() && (want_out.is_null() || want_out.is_number())) {
        return Some(format!("out: got {out}, want {want_out}"));
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
    assert_eq!(header["domain"], "codec");
    codec::init();
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            fails.push(format!("{line}\n    {why}"));
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
fn govec_codec() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_CODEC_FILES") {
        for f in files.split(':').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
