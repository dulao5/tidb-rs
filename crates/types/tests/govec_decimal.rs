//! Replays the Go-generated vectors `tests/vectors/v8.5.8/decimal.jsonl` (tracker
//! `govec`, see the README next to the file) against the Rust port.
//!
//! Rules (tracker `govec/README.md`): outputs (`s`, `str`, internal state `st`, ints, hex,
//! float strings) must be equal; errors must match by code (the message is compared too);
//! `err.name == "panic"` records must panic in Rust as well (checked with `catch_unwind`);
//! `err.name == "other"` (Go encoding/json errors) must be an error without a MySQL code.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use tidb_types::{
    Error, MODE_CEILING, MODE_HALF_UP, MODE_TRUNCATE, MyDecimal, RoundMode, decimal_add,
    decimal_bin_size, decimal_div, decimal_mod, decimal_mul, decimal_neg, decimal_peak,
    decimal_sub, gostd, new_max_or_min_dec,
};

const VECTORS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/vectors/v8.5.8/decimal.jsonl"
);

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn parse_float(s: &str) -> f64 {
    match s {
        "NaN" => f64::NAN,
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        _ => s.parse().unwrap(),
    }
}

fn mode(s: &str) -> RoundMode {
    match s {
        "HalfUp" => MODE_HALF_UP,
        "Truncate" => MODE_TRUNCATE,
        "Ceiling" => MODE_CEILING,
        _ => panic!("bad round mode {s}"),
    }
}

/// govec's `Dec`: ToString(), String() and the internal state read through MarshalJSON.
fn dec_out(d: &MyDecimal) -> Value {
    let j: Value = serde_json::from_slice(&d.marshal_json().unwrap()).unwrap();
    let mut st = vec![
        j["DigitsInt"].clone(),
        j["DigitsFrac"].clone(),
        j["ResultFrac"].clone(),
        json!(i64::from(j["Negative"].as_bool().unwrap())),
    ];
    st.extend(j["WordBuf"].as_array().unwrap().iter().cloned());
    json!({
        "s": String::from_utf8(d.to_string()).unwrap(),
        "str": d.string(),
        "st": st,
    })
}

struct Args<'a>(&'a [Value]);

impl Args<'_> {
    fn str(&self, i: usize) -> &str {
        self.0[i].as_str().unwrap()
    }
    fn int(&self, i: usize) -> isize {
        self.0[i].as_i64().unwrap() as isize
    }
    /// Decimal argument: FromString, error ignored (as govec does).
    fn dec(&self, i: usize) -> MyDecimal {
        let mut d = MyDecimal::default();
        let _ = d.from_string(self.str(i).as_bytes());
        d
    }
}

fn exec(op: &str, args: &[Value]) -> (Value, Result<(), Error>) {
    let a = Args(args);
    match op {
        "from_string" => {
            let mut d = MyDecimal::default();
            let err = d.from_string(a.str(0).as_bytes());
            (dec_out(&d), err)
        }
        "from_int" => {
            let mut d = MyDecimal::default();
            d.from_int(args[0].as_i64().unwrap());
            (dec_out(&d), Ok(()))
        }
        "from_uint" => {
            let mut d = MyDecimal::default();
            d.from_uint(args[0].as_u64().unwrap());
            (dec_out(&d), Ok(()))
        }
        "from_float64" => {
            let mut d = MyDecimal::default();
            let err = d.from_float64(parse_float(a.str(0)));
            (dec_out(&d), err)
        }
        "to_int" => {
            let (v, err) = a.dec(0).to_int();
            (json!(v), err)
        }
        "to_uint" => {
            let (v, err) = a.dec(0).to_uint();
            (json!(v), err)
        }
        "to_float64" => {
            let (v, err) = a.dec(0).to_float64();
            (json!(gostd::format_float_g(v)), err)
        }
        "round" => {
            let d = a.dec(0);
            let mut to = MyDecimal::default();
            let err = d.round(&mut to, a.int(1), mode(a.str(2)));
            (dec_out(&to), err)
        }
        "round_inplace" => {
            let mut d = a.dec(0);
            let err = d.round_in_place(a.int(1), mode(a.str(2)));
            (dec_out(&d), err)
        }
        "shift" => {
            let mut d = a.dec(0);
            let err = d.shift(a.int(1));
            (dec_out(&d), err)
        }
        "add" | "sub" | "mul" | "mod" => {
            let (x, y) = (a.dec(0), a.dec(1));
            let mut to = MyDecimal::default();
            let err = match op {
                "add" => decimal_add(&x, &y, &mut to),
                "sub" => decimal_sub(&x, &y, &mut to),
                "mul" => decimal_mul(&x, &y, &mut to),
                _ => decimal_mod(&x, &y, &mut to),
            };
            (dec_out(&to), err)
        }
        "div" => {
            let (x, y) = (a.dec(0), a.dec(1));
            let mut to = MyDecimal::default();
            let err = decimal_div(&x, &y, &mut to, a.int(2));
            (dec_out(&to), err)
        }
        "neg" => (dec_out(&decimal_neg(&a.dec(0))), Ok(())),
        "compare" => (json!(a.dec(0).compare(&a.dec(1))), Ok(())),
        "to_bin" => {
            let (b, err) = a.dec(0).to_bin(a.int(1), a.int(2));
            (json!(hex(&b)), err)
        }
        "from_bin" => {
            let mut d = MyDecimal::default();
            let (n, err) = d.from_bin(&unhex(a.str(0)), a.int(1), a.int(2));
            (json!({"n": n, "dec": dec_out(&d)}), err)
        }
        "bin_size" => match decimal_bin_size(a.int(0), a.int(1)) {
            Ok(n) => (json!(n), Ok(())),
            Err(e) => (json!(0), Err(e)),
        },
        "peak" => match decimal_peak(&unhex(a.str(0))) {
            Ok(n) => (json!(n), Ok(())),
            Err(e) => (json!(0), Err(e)),
        },
        "to_hash_key" => {
            let (b, err) = a.dec(0).to_hash_key();
            (json!(hex(&b)), err)
        }
        "props" => {
            let d = a.dec(0);
            let (p, f) = d.precision_and_frac();
            (
                json!({
                    "digits_int": d.get_digits_int(),
                    "digits_frac": d.get_digits_frac(),
                    "negative": d.is_negative(),
                    "zero": d.is_zero(),
                    "precision": p,
                    "frac": f,
                }),
                Ok(()),
            )
        }
        "max_or_min" => (
            dec_out(&new_max_or_min_dec(
                args[0].as_bool().unwrap(),
                a.int(1),
                a.int(2),
            )),
            Ok(()),
        ),
        "marshal_json" => {
            let j = a.dec(0).marshal_json();
            (
                json!(String::from_utf8(j.clone().unwrap_or_default()).unwrap()),
                j.map(|_| ()),
            )
        }
        "unmarshal_json" => {
            let mut d = MyDecimal::default();
            let err = d.unmarshal_json(a.str(0).as_bytes());
            (dec_out(&d), err)
        }
        _ => panic!("unknown op {op}"),
    }
}

/// Compares one record; `None` if it matches, otherwise a description of the difference.
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
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            return Some(format!("Rust panicked: {msg}"));
        }
    };
    let mut diffs = Vec::new();
    if out != rec["out"] {
        diffs.push(format!("out: got {out}, want {}", rec["out"]));
    }
    match (&err, want_err) {
        (Ok(()), Value::Null) => {}
        (Ok(()), w) => diffs.push(format!("err: got nil, want {w}")),
        (Err(e), Value::Null) => diffs.push(format!("err: got {e}, want nil")),
        (Err(e), w) if w["name"] == "other" => {
            if e.code() != 0 {
                diffs.push(format!("err: got {e}, want a non-terror error {w}"));
            }
        }
        (Err(e), w) => {
            if u64::from(e.code()) != w["code"].as_u64().unwrap() || e.to_string() != w["msg"] {
                diffs.push(format!("err: got [{}] {e}, want {w}", e.code()));
            }
        }
    }
    if diffs.is_empty() {
        None
    } else {
        Some(diffs.join("; "))
    }
}

#[test]
fn govec_decimal() {
    let text = std::fs::read_to_string(VECTORS).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "decimal");
    assert_eq!(header["tidb"], "v8.5.8");

    // Expected panics would print their messages; keep the output readable.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    // op -> (records, passed, of which Go panics)
    let mut stats: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap().to_string();
        let e = stats.entry(op.clone()).or_default();
        e.0 += 1;
        if rec["err"]["name"] == "panic" {
            e.2 += 1;
        }
        match check(&rec) {
            None => e.1 += 1,
            Some(diff) => {
                failures.push(format!("{op} {} (src {}): {diff}", rec["args"], rec["src"]))
            }
        }
    }
    std::panic::set_hook(hook);

    assert_eq!(n, header["records"].as_u64().unwrap() as usize);
    eprintln!(
        "{:<16} {:>7} {:>7} {:>7}",
        "op", "records", "passed", "panics"
    );
    for (op, (total, passed, panics)) in &stats {
        eprintln!("{op:<16} {total:>7} {passed:>7} {panics:>7}");
    }
    let passed: usize = stats.values().map(|s| s.1).sum();
    eprintln!("{:<16} {:>7} {:>7}", "total", n, passed);
    for f in failures.iter().take(40) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "{} of {n} records differ from Go",
        failures.len()
    );
}
