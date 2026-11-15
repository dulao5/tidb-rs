//! Replays the govec `protocol` vectors (tidb-rs-tracker `govec/domains/protocol.go`): the
//! MySQL protocol encoding of results (column definitions, `DumpTextRow` /
//! `DumpBinaryRow` over every column type and `@@character_set_results`,
//! `ConvertColumnInfo`, `AppendFormatFloat`) and the binary parameters of
//! `COM_STMT_EXECUTE` (`parseBinaryParams`, then `ExecBinaryParam`). Spec
//! `specs/D03.1-server/spec.md` §5.

use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_chunk::chunk::{Chunk, new_chunk_with_capacity};
use tidb_executor::glue::ResultField;
use tidb_expression::param::BinaryParam;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_server::column::{
    Info, ResultEncoder, convert_column_info, dump_binary_row, dump_text_row,
};
use tidb_server::params::parse_binary_params;
use tidb_server::util::{InputDecoder, append_format_float};
use tidb_types::datum::Datum;
use tidb_types::enum_type::Enum;
use tidb_types::mydecimal::MyDecimal;
use tidb_types::set_type::Set;

mod hex {
    pub fn encode(b: impl AsRef<[u8]>) -> String {
        b.as_ref().iter().map(|x| format!("{x:02x}")).collect()
    }
    pub fn decode(s: &str) -> Result<Vec<u8>, std::num::ParseIntError> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect()
    }
}

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

/// A `vec.GoString`: a JSON string, or `{"hex": ...}` for bytes that are not UTF-8.
fn go_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => hex::decode(m["hex"].as_str().unwrap()).unwrap(),
        _ => panic!("not a GoString: {v}"),
    }
}

/// The JSON of a GoString holding `b`.
fn go_string(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({ "hex": hex::encode(b) }),
    }
}

fn err_out(e: &Error) -> Value {
    json!({"err": {"code": e.code(), "msg": go_string(e.to_string().as_bytes())}})
}

fn info(v: &Value) -> Info {
    Info {
        default_value: v["default"].as_str().map(str::to_owned),
        schema: v["schema"].as_str().unwrap().to_owned(),
        table: v["table"].as_str().unwrap().to_owned(),
        org_table: v["org_table"].as_str().unwrap().to_owned(),
        name: v["name"].as_str().unwrap().to_owned(),
        org_name: v["org_name"].as_str().unwrap().to_owned(),
        column_length: v["column_length"].as_u64().unwrap() as u32,
        charset: v["charset"].as_u64().unwrap() as u16,
        flag: v["flag"].as_u64().unwrap() as u16,
        decimal: v["decimal"].as_u64().unwrap() as u8,
        tp: v["type"].as_u64().unwrap() as u8,
    }
}

fn str_u64(v: &Value) -> u64 {
    v.as_str().unwrap().parse().unwrap()
}

fn append(chk: &mut Chunk, i: usize, v: &Value) {
    let Some(m) = v.as_object() else {
        chk.append_null(i);
        return;
    };
    if let Some(x) = m.get("i") {
        chk.append_int64(i, x.as_str().unwrap().parse().unwrap());
    } else if let Some(x) = m.get("f32") {
        chk.append_float32(i, f32::from_bits(x.as_u64().unwrap() as u32));
    } else if let Some(x) = m.get("f64") {
        chk.append_float64(i, f64::from_bits(str_u64(x)));
    } else if let Some(x) = m.get("dec") {
        let mut d = MyDecimal::default();
        let _ = d.from_string(x.as_str().unwrap().as_bytes());
        chk.append_my_decimal(i, &d);
    } else if let Some(x) = m.get("b") {
        chk.append_bytes(i, &hex::decode(x.as_str().unwrap()).unwrap());
    } else if let Some(x) = m.get("t") {
        let t: Vec<isize> = x
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_i64().unwrap() as isize)
            .collect();
        let ct = tidb_types::time::from_date(t[0], t[1], t[2], t[3], t[4], t[5], t[6]);
        chk.append_time(
            i,
            tidb_types::time::new_time(
                ct,
                m["tp"].as_u64().unwrap() as u8,
                m["fsp"].as_i64().unwrap() as isize,
            ),
        );
    } else if let Some(x) = m.get("d") {
        chk.append_duration(
            i,
            tidb_types::time::Duration {
                duration: x.as_str().unwrap().parse().unwrap(),
                fsp: 0,
            },
        );
    } else if let Some(x) = m.get("enum").or_else(|| m.get("set")) {
        let a = x.as_array().unwrap();
        let (name, value) = (go_bytes(&a[0]), a[1].as_u64().unwrap());
        if m.contains_key("enum") {
            chk.append_enum(i, &Enum { name, value });
        } else {
            chk.append_set(i, &Set { name, value });
        }
    } else if let Some(x) = m.get("json") {
        let j = tidb_types::json::parse_binary_json_from_string(x.as_str().unwrap().as_bytes())
            .unwrap();
        chk.append_json(i, &j);
    } else if let Some(x) = m.get("vec") {
        let fs: Vec<f32> = x
            .as_array()
            .unwrap()
            .iter()
            .map(|b| f32::from_bits(b.as_u64().unwrap() as u32))
            .collect();
        let v = tidb_types::vector::create_vector_float32(&fs).unwrap();
        chk.append_vector_float32(i, &v);
    } else {
        panic!("value {v}");
    }
}

fn op_row(a: &[Value]) -> Value {
    let binary = a[0].as_bool().unwrap();
    let mut enc = ResultEncoder::new(a[1].as_str().unwrap());
    let cols: Vec<Info> = a[2].as_array().unwrap().iter().map(info).collect();
    let fts: Vec<FieldType> = cols
        .iter()
        .map(|c| {
            let mut ft = FieldType::new(c.tp);
            ft.set_flag(c.flag as usize);
            ft
        })
        .collect();
    let mut chk = new_chunk_with_capacity(&fts, 1);
    for (i, v) in a[3].as_array().unwrap().iter().enumerate() {
        append(&mut chk, i, v);
    }
    let mut buf = Vec::new();
    let row = chk.get_row(0);
    let r = if binary {
        dump_binary_row(&mut buf, &cols, &row, Some(&mut enc))
    } else {
        dump_text_row(&mut buf, &cols, &row, Some(&mut enc))
    };
    match r {
        Ok(()) => json!(hex::encode(buf)),
        Err(e) => err_out(&e),
    }
}

fn op_convert(a: &[Value]) -> Value {
    let f = &a[0];
    let mut ft = FieldType::new(f["tp"].as_u64().unwrap() as u8);
    ft.set_flag(f["flag"].as_u64().unwrap() as usize);
    ft.set_flen(f["flen"].as_i64().unwrap() as isize);
    ft.set_decimal(f["decimal"].as_i64().unwrap() as isize);
    ft.set_charset(f["charset"].as_str().unwrap());
    let fld = ResultField {
        column_name: CIStr::new(f["name"].as_str().unwrap()),
        column_type: ft,
        column_as_name: CIStr::new(f["as_name"].as_str().unwrap()),
        empty_org_name: f["empty_org_name"].as_bool().unwrap(),
        table_name: CIStr::new(f["table"].as_str().unwrap()),
        table_as_name: CIStr::new(f["table_as"].as_str().unwrap()),
        db_name: CIStr::new(f["db"].as_str().unwrap()),
    };
    let ci = convert_column_info(&fld);
    json!({"schema": ci.schema, "table": ci.table, "org_table": ci.org_table, "name": ci.name,
        "org_name": ci.org_name, "column_length": ci.column_length, "charset": ci.charset,
        "flag": ci.flag, "decimal": ci.decimal, "type": ci.tp})
}

#[allow(non_upper_case_globals)]
fn datum(d: &Datum) -> Value {
    use tidb_types::datum::*;
    let k = d.kind();
    let mut out = serde_json::Map::new();
    out.insert("k".into(), json!(k));
    match k {
        KindInt64 => {
            out.insert("v".into(), json!(d.get_int64().to_string()));
        }
        KindUint64 => {
            out.insert("v".into(), json!(d.get_uint64().to_string()));
        }
        KindFloat32 => {
            out.insert(
                "v".into(),
                json!((d.get_float64() as f32).to_bits().to_string()),
            );
        }
        KindFloat64 => {
            out.insert("v".into(), json!(d.get_float64().to_bits().to_string()));
        }
        KindString | KindBytes => {
            out.insert("v".into(), json!(hex::encode(d.get_bytes())));
            // Go's `GetBytes() == nil`: GetBytes never returns nil (an empty slice instead).
            out.insert("nil".into(), json!(false));
        }
        KindMysqlDecimal => {
            out.insert("v".into(), json!(d.get_mysql_decimal().string()));
        }
        KindMysqlTime => {
            let t = d.get_mysql_time();
            out.insert("v".into(), go_string(&t.string()));
            out.insert("tp".into(), json!(t.tp()));
            out.insert("fsp".into(), json!(t.fsp()));
        }
        KindMysqlDuration => {
            let dur = d.get_mysql_duration();
            out.insert("v".into(), json!(dur.duration.to_string()));
            out.insert("fsp".into(), json!(dur.fsp));
        }
        _ => {}
    }
    Value::Object(out)
}

fn op_params(a: &[Value]) -> Value {
    let n = a[0].as_u64().unwrap() as usize;
    let dec = |v: &Value| hex::decode(v.as_str().unwrap()).unwrap();
    let mut bound: Vec<Option<Vec<u8>>> = vec![None; n];
    for (i, b) in a[1].as_array().unwrap().iter().enumerate() {
        if !b.is_null() {
            bound[i] = Some(dec(b));
        }
    }
    let enc = InputDecoder::new(a[5].as_str().unwrap());
    let mut params = vec![BinaryParam::default(); n];
    if let Err(e) = parse_binary_params(
        &mut params,
        &bound,
        &dec(&a[2]),
        &dec(&a[3]),
        &dec(&a[4]),
        Some(&enc),
    ) {
        return json!({ "parse": err_out(&e) });
    }
    let ps: Vec<Value> = params
        .iter()
        .map(|p| json!({"tp": p.tp, "unsigned": p.is_unsigned, "null": p.is_null, "val": hex::encode(&p.val)}))
        .collect();
    let tc = tidb_types::context::new_context(
        tidb_types::context::Flags(a[6].as_u64().unwrap() as _),
        tidb_base::gostd::time::utc(),
        Arc::new(tidb_types::context::IgnoreWarn),
    );
    let exec = match tidb_expression::util::exec_binary_param(&tc, &params) {
        Err(e) => err_out(&e),
        Ok(cs) => Value::Array(
            cs.iter()
                .map(|c| {
                    let ft = &c.ret_type;
                    json!({"datum": datum(&c.value), "tp": ft.get_type(), "flag": ft.get_flag(),
                        "flen": ft.get_flen(), "decimal": ft.get_decimal(),
                        "charset": ft.get_charset(), "collate": ft.get_collate()})
                })
                .collect(),
        ),
    };
    json!({"parse": ps, "exec": exec})
}

fn run(op: &str, a: &[Value]) -> Value {
    match op {
        "column" => {
            let i = info(&a[0]);
            let mut enc = ResultEncoder::new(a[1].as_str().unwrap());
            let mut buf = Vec::new();
            if a[2].as_bool().unwrap() {
                i.dump_with_default(&mut buf, Some(&mut enc));
            } else {
                i.dump(&mut buf, Some(&mut enc));
            }
            json!(hex::encode(buf))
        }
        "convert" => op_convert(a),
        "row" => op_row(a),
        "float" => {
            let f = f64::from_bits(str_u64(&a[0]));
            let mut out = Vec::new();
            append_format_float(
                &mut out,
                f,
                a[1].as_i64().unwrap() as isize,
                a[2].as_u64().unwrap() as u8,
            );
            json!(String::from_utf8(out).unwrap())
        }
        "params" => op_params(a),
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_protocol() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/protocol.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "protocol");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let (mut n, mut bad) = (0, Vec::new());
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let a = rec["args"].as_array().unwrap();
        let got = catch_unwind(AssertUnwindSafe(|| run(op, a)));
        let ok = match (&got, rec["err"]["name"].as_str()) {
            (Err(_), Some("panic")) => true,
            (Ok(v), None) => *v == rec["out"],
            _ => false,
        };
        if !ok {
            bad.push(format!(
                "{line}\n    got {:?}",
                got.as_ref().map_err(|_| "panic")
            ));
        }
    }
    std::panic::set_hook(prev);
    assert!(
        bad.is_empty(),
        "{} of {n} records differ:\n{}",
        bad.len(),
        bad[..bad.len().min(40)].join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}
