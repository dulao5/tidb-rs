//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/vector.jsonl.gz`
//! (tracker `govec`, domain `vector`: Go TiDB v8.5.8 `pkg/types` vector.go and
//! vector_functions.go, with jsoniter's parser) against the Rust port.
//!
//! Vectors are their serialized bytes as hex; float32 inputs and float64 outputs are IEEE
//! bits. Every record's `out` must be equal as JSON, errors must match by message, and
//! `err.name == "panic"` records must panic in Rust too. The distance records depend on
//! the target (M-FMA), so each Rust target replays the file of its Go target.

use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tidb_base::Error;
use tidb_types::vector::{self as v, VectorFloat32, ZeroVectorFloat32};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/vector.jsonl.gz",
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

fn vec_out(x: &VectorFloat32<'_>) -> Value {
    json!({
        "data": hex(x.zero_copy_serialize()),
        "s": gs(x.string().as_bytes()),
        "ts": gs(x.truncated_string().as_bytes()),
        "len": x.len(),
        "zero": x.is_zero_value(),
        "size": x.serialized_size(),
        "mem": x.estimated_mem_usage(),
    })
}

/// A vector result: Go returns `ZeroVectorFloat32` with an error.
fn vec_res(r: Result<VectorFloat32<'_>, Error>) -> (Value, Option<Error>) {
    match r {
        Ok(x) => (vec_out(&x), None),
        Err(e) => (vec_out(&ZeroVectorFloat32), Some(e)),
    }
}

fn f64_res(r: Result<f64, Error>) -> (Value, Option<Error>) {
    match r {
        Ok(d) => (json!(d.to_bits()), None),
        Err(e) => (json!(0u64), Some(e)),
    }
}

fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>) {
    let bytes = |i: usize| unhex(args[i].as_str().unwrap());
    let with_vec = |i: usize, f: &mut dyn FnMut(&VectorFloat32<'_>)| {
        let b = bytes(i);
        let (x, rest) = v::zero_copy_deserialize_vector_float32(&b).unwrap();
        assert!(rest.is_empty());
        f(&x)
    };
    match op {
        "zero_vec" => (vec_out(&ZeroVectorFloat32), None),
        "parse" => vec_res(v::parse_vector_float32(&ungs(&args[0]))),
        "create" | "must_create" => {
            let fs: Vec<f32> = args[0]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| f32::from_bits(b.as_u64().unwrap() as u32))
                .collect();
            if op == "must_create" {
                (vec_out(&v::must_create_vector_float32(&fs)), None)
            } else {
                vec_res(v::create_vector_float32(&fs))
            }
        }
        "check_dim" => (
            Value::Null,
            v::check_vector_dim_valid(args[0].as_i64().unwrap() as isize).err(),
        ),
        "check_fit" => {
            let mut r = None;
            with_vec(0, &mut |x| {
                r = x
                    .check_dims_fit_column(args[1].as_i64().unwrap() as isize)
                    .err()
            });
            (Value::Null, r)
        }
        "deserialize" => {
            let b = bytes(0);
            let n = v::peek_bytes_as_vector_float32(&b);
            match v::zero_copy_deserialize_vector_float32(&b) {
                Ok((x, rest)) => (
                    json!({ "n": n.unwrap(), "vec": vec_out(&x), "rest": hex(rest) }),
                    None,
                ),
                Err(e) => {
                    assert!(n.is_err());
                    (
                        json!({ "n": 0, "vec": vec_out(&ZeroVectorFloat32), "rest": hex(&b) }),
                        Some(e),
                    )
                }
            }
        }
        "serialize_to" => {
            let pre = bytes(0);
            let mut out = Value::Null;
            with_vec(1, &mut |x| out = json!(hex(&x.serialize_to(pre.clone()))));
            (out, None)
        }
        "clone" => {
            let mut out = Value::Null;
            with_vec(0, &mut |a| {
                let mut c = a.clone();
                if !c.elements().is_empty() {
                    // The clone does not share memory with the original.
                    c.elements_mut().set(0, 42.0);
                }
                out = json!({ "orig": vec_out(a), "clone": vec_out(&c) });
            });
            (out, None)
        }
        "distance" | "arith" => {
            let (a, b) = (bytes(1), bytes(2));
            let (a, _) = v::zero_copy_deserialize_vector_float32(&a).unwrap();
            let (b, _) = v::zero_copy_deserialize_vector_float32(&b).unwrap();
            match args[0].as_str().unwrap() {
                "l2_squared" => f64_res(a.l2_squared_distance(&b)),
                "l2" => f64_res(a.l2_distance(&b)),
                "inner_product" => f64_res(a.inner_product(&b)),
                "negative_inner_product" => f64_res(a.negative_inner_product(&b)),
                "cosine" => f64_res(a.cosine_distance(&b)),
                "l1" => f64_res(a.l1_distance(&b)),
                "add" => vec_res(a.add(&b)),
                "sub" => vec_res(a.sub(&b)),
                "mul" => vec_res(a.mul(&b)),
                o => panic!("unknown {op} {o}"),
            }
        }
        "l2_norm" => {
            let mut out = Value::Null;
            with_vec(0, &mut |x| out = json!(x.l2_norm().to_bits()));
            (out, None)
        }
        "compare" => {
            let (a, b) = (bytes(0), bytes(1));
            let (a, _) = v::zero_copy_deserialize_vector_float32(&a).unwrap();
            let (b, _) = v::zero_copy_deserialize_vector_float32(&b).unwrap();
            (json!(a.compare(&b)), None)
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
fn govec_vector() {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "vector");
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
