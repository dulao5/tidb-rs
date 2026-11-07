//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/json.jsonl.gz` (tracker
//! `govec`, domain `json`: Go TiDB v8.5.8 `pkg/types/json_*.go`, block D01.5, exception A4)
//! against the Rust port.
//!
//! A JSON argument is a text (GoString, parsed with `parse_binary_json_from_string`) or a
//! `{"k": kind, "v": ...}` spec built with `create_binary_json`; an output JSON is
//! `{"tc", "hex", "s"}` (type code, value bytes, `String()`). Every record's `out` must be
//! equal as JSON, errors must match by message, and Go panics must panic in Rust too.

use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tidb_base::Error;
use tidb_types::core_time::CoreTime;
use tidb_types::json::{self as j, BinaryJSON, JSONPathExpression, JSONValue, Opaque};
use tidb_types::time::{Duration, new_time};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/json.jsonl.gz",
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

fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_owned()),
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

fn is_spec(v: &Value) -> bool {
    v.as_object().is_some_and(|m| m.contains_key("k"))
}

fn jout(bj: &BinaryJSON<'_>) -> Value {
    json!({"tc": bj.type_code, "hex": hex(&bj.value), "s": gs(&bj.string())})
}

fn value_of_spec(v: &Value) -> JSONValue<'static> {
    if !is_spec(v) {
        return JSONValue::BinaryJSON(doc_of(v));
    }
    let x = &v["v"];
    match v["k"].as_str().unwrap() {
        "nil" => JSONValue::Nil,
        "bool" => JSONValue::Bool(x.as_bool().unwrap()),
        "int64" => JSONValue::Int64(x.as_i64().unwrap()),
        "uint64" => JSONValue::Uint64(x.as_u64().unwrap()),
        "float64" => JSONValue::Float64(f64::from_bits(x.as_u64().unwrap())),
        "string" => JSONValue::String(ungs(x)),
        "opaque" => JSONValue::Opaque(Opaque {
            type_code: x[0].as_u64().unwrap() as u8,
            buf: Cow::Owned(unhex(x[1].as_str().unwrap())),
        }),
        "time" => JSONValue::Time(new_time(
            CoreTime(x[0].as_u64().unwrap()),
            x[1].as_u64().unwrap() as u8,
            6,
        )),
        "duration" => JSONValue::Duration(Duration {
            duration: x[0].as_i64().unwrap(),
            fsp: x[1].as_i64().unwrap() as isize,
        }),
        "array" => JSONValue::Array(x.as_array().unwrap().iter().map(value_of_spec).collect()),
        "object" => {
            let mut m = BTreeMap::new();
            for kv in x.as_array().unwrap() {
                m.insert(ungs(&kv[0]), value_of_spec(&kv[1]));
            }
            JSONValue::Object(m)
        }
        k => panic!("unknown kind {k}"),
    }
}

fn doc_of(v: &Value) -> BinaryJSON<'static> {
    if is_spec(v) {
        return j::create_binary_json(value_of_spec(v));
    }
    j::parse_binary_json_from_string(&ungs(v)).unwrap()
}

fn paths_of(v: &Value) -> Result<Vec<JSONPathExpression>, Error> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|p| j::parse_json_path_expr(&ungs(p)))
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()))
}

fn res(r: Result<Value, Error>) -> (Value, Option<Error>) {
    match r {
        Ok(v) => (v, None),
        Err(e) => (Value::Null, Some(e)),
    }
}

fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>) {
    let doc = |i: usize| doc_of(&args[i]);
    match op {
        "parse" => res(j::parse_binary_json_from_string(&ungs(&args[0]))
            .map(|bj| json!({"j": jout(&bj), "type": bj.r#type(), "depth": bj.get_elem_depth()}))),
        "create" => res(
            j::create_binary_json_with_check(&value_of_spec(&args[0])).map(|bj| {
                let c = bj.copy();
                let mut out = json!({"j": jout(&c)});
                match bj.marshal_json() {
                    Ok(m) => out["marshal"] = gs(&m),
                    Err(e) => {
                        out["marshal"] = gs(b"");
                        out["marshal_err"] = gs(&e.error());
                    }
                }
                out
            }),
        ),
        "type" => (json!(doc(0).r#type()), None),
        "unquote" => match doc(0).unquote() {
            Ok(s) => (gs(&s), None),
            Err(e) => (gs(b""), Some(e)),
        },
        "unquote_string" => match j::unquote_string(&ungs(&args[0])) {
            Ok(s) => (gs(&s), None),
            Err(e) => (gs(b""), Some(e)),
        },
        "hash" => (json!(hex(&doc(0).hash_value(Vec::new()))), None),
        "compare" => (
            json!(j::compare_binary_json(doc(0).as_ref(), doc(1).as_ref())),
            None,
        ),
        "is_zero" => (json!(doc(0).is_zero()), None),
        "depth" => (json!(doc(0).get_elem_depth()), None),
        "keys" => (jout(&doc(0).get_keys()), None),
        "get_value" => {
            let bj = doc(0);
            let v = match bj.get_value() {
                Some(JSONValue::Int64(v)) => json!({"int64": v}),
                Some(JSONValue::Uint64(v)) => json!({"uint64": v}),
                Some(JSONValue::Float64(v)) => json!({"float64": v.to_bits()}),
                Some(JSONValue::String(v)) => json!({"string": gs(&v)}),
                Some(JSONValue::Duration(v)) => json!({"duration": [v.duration, v.fsp]}),
                Some(JSONValue::Time(v)) => json!({"time": gs(&v.string())}),
                None => Value::Null,
                Some(_) => panic!("unexpected GetValue type"),
            };
            (v, None)
        }
        "peek" => match j::peek_bytes_as_json(&unhex(args[0].as_str().unwrap())) {
            Ok(n) => (json!(n), None),
            Err(e) => (json!(0), Some(e)),
        },
        "path_parse" => res(j::parse_json_path_expr(&ungs(&args[0]))
            .map(|pe| json!({"s": gs(&pe.string()), "multi": pe.could_match_multiple_values()}))),
        "extract" => {
            let bj = doc(0);
            res(paths_of(&args[1]).map(|ps| {
                let (ret, found) = bj.extract(&ps);
                if found {
                    json!({"found": true, "j": jout(&ret)})
                } else {
                    json!({"found": false})
                }
            }))
        }
        "modify" => {
            let bj = doc(0);
            res(paths_of(&args[1]).and_then(|ps| {
                let vals: Vec<BinaryJSON<'static>> =
                    args[2].as_array().unwrap().iter().map(doc_of).collect();
                bj.modify(&ps, &vals, args[3].as_u64().unwrap() as u8)
                    .map(|r| jout(&r))
            }))
        }
        "array_insert" => {
            let bj = doc(0);
            res(paths_of(&args[1])
                .and_then(|ps| bj.array_insert(&ps[0], &doc(2)).map(|r| jout(&r))))
        }
        "remove" => {
            let bj = doc(0);
            res(paths_of(&args[1]).and_then(|ps| bj.remove(&ps).map(|r| jout(&r))))
        }
        "merge" => {
            let bjs: Vec<BinaryJSON<'static>> =
                args[0].as_array().unwrap().iter().map(doc_of).collect();
            (jout(&j::merge_binary_json(&bjs)), None)
        }
        "merge_patch" => {
            let bjs: Vec<Option<BinaryJSON<'static>>> = args[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| if v.is_null() { None } else { Some(doc_of(v)) })
                .collect();
            res(j::merge_patch_binary_json(&bjs).map(|r| match r {
                Some(r) => jout(&r),
                None => Value::Null,
            }))
        }
        "contains" => (
            json!(j::contains_binary_json(doc(0).as_ref(), doc(1).as_ref())),
            None,
        ),
        "overlaps" => (
            json!(j::overlaps_binary_json(doc(0).as_ref(), doc(1).as_ref())),
            None,
        ),
        "search" => {
            let bj = doc(0);
            res(paths_of(&args[4]).and_then(|ps| {
                bj.search(
                    args[1].as_str().unwrap(),
                    &ungs(&args[2]),
                    args[3].as_u64().unwrap() as u8,
                    &ps,
                )
                .map(|(r, is_null)| {
                    if is_null {
                        json!({"null": true})
                    } else {
                        json!({"null": false, "j": jout(&r)})
                    }
                })
            }))
        }
        "walk" => {
            let bj = doc(0);
            let limit = args[2].as_u64().unwrap() as usize;
            match paths_of(&args[1]) {
                Err(e) => (Value::Null, Some(e)),
                Ok(ps) => {
                    let mut visited = Vec::new();
                    let r = bj.walk(
                        &mut |p, v| {
                            visited.push(json!([gs(&p.string()), jout(&v)]));
                            Ok(visited.len() >= limit)
                        },
                        &ps,
                    );
                    let out = if visited.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(visited)
                    };
                    (out, r.err())
                }
            }
        }
        _ => panic!("unknown op {op}"),
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
    if out != rec["out"] {
        return Some(format!("out: got {out}, want {}", rec["out"]));
    }
    let got_err = err.as_ref().map(|e| gs(&e.error()));
    let want = if want_err.is_null() {
        None
    } else {
        Some(want_err["msg"].clone())
    };
    if got_err != want {
        return Some(format!("err: got {got_err:?}, want {want:?}"));
    }
    None
}

#[test]
fn govec_json() {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "json");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            let mut l = line.to_owned();
            l.truncate(400);
            fails.push(format!("{l}\n    {why}"));
        }
    }
    std::panic::set_hook(prev);
    assert!(
        fails.is_empty(),
        "{} of {n} records differ:\n{}",
        fails.len(),
        fails
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}
