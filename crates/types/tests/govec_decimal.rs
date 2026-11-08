//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/decimal.jsonl.gz`
//! (tracker `govec`, see the README there) against the Rust port.
//!
//! The vectors depend on the target: Go gc fuses float `x*y + z` into FMA on arm64 and not
//! on amd64 (GOAMD64=v1), which changes `ToFloat64` of some `DecimalDiv` results (mapping
//! rule `M-FMA`). The test replays the set of the Go target matching the Rust target arch:
//! `x86_64` -> `amd64`, `aarch64` -> `arm64`; other arches have no vector set and fail.
//!
//! Rules (tracker `govec/README.md`): outputs (`s`, `str`, internal state `st`, ints, hex,
//! float strings) must be equal; errors must match by code (the message is compared too);
//! `err.name == "panic"` records must panic in Rust as well (checked with `catch_unwind`);
//! `err.name == "other"` (Go encoding/json errors) must be an error without a MySQL code
//! and, for the `unmarshal_json*` ops, with Go's message (tidb-base ports `encoding/json`'s
//! syntax and type error texts).
//!
//! `*_alias` ops are Go calls whose `to` is one of the inputs (`f(x, y, x)`, `f(x, y, y)`,
//! `f(x, x, x)`); per mapping rule `M-ALIAS` the Rust caller copies the aliased input first
//! (`MyDecimal: Copy`). `round_inplace` uses the in-place entry `round_in_place`.
//! `<producer>_then_<consumer>` ops hand the producer's result object (not re-parsed) to the
//! consumer as its first decimal; err is the producer's error, else the consumer's.
//! `dirty_<op>` ops run op on a receiver / `to` that `from_string(args[0])` left behind
//! (Go's From* methods do not reset the fields they do not write).

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use tidb_types::{
    Error, MODE_CEILING, MODE_HALF_UP, MODE_TRUNCATE, MyDecimal, RoundMode, decimal_add,
    decimal_bin_size, decimal_div, decimal_mod, decimal_mul, decimal_neg, decimal_peak,
    decimal_sub, gostd, new_max_or_min_dec,
};

/// Go target (govec `vec.Target`) whose vectors this Rust target must reproduce.
const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/decimal.jsonl.gz",
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
    check_wire(d, &st);
    json!({
        "s": String::from_utf8(d.to_string()).unwrap(),
        "str": d.string(),
        "st": st,
    })
}

/// Every decimal result also checks the chunk wire image: `as_wire` must be the 40-byte Go
/// struct image of the state `st` (compared with Go's `st` by the record check), and
/// `from_wire` must give the same object back.
fn check_wire(d: &MyDecimal, st: &[Value]) {
    let mut want = Vec::with_capacity(40);
    for v in &st[..4] {
        want.push(v.as_i64().unwrap() as u8);
    }
    for v in &st[4..] {
        want.extend_from_slice(&(v.as_i64().unwrap() as i32).to_le_bytes());
    }
    let w = d.as_wire();
    assert_eq!(&w[..], &want[..], "as_wire of {st:?}");
    assert_eq!(
        MyDecimal::from_wire(&w).as_ref(),
        Ok(d),
        "from_wire of {st:?}"
    );
    WIRE_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

static WIRE_CHECKS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Arguments of one op. In the consumer step of a chained op, `first` is the producer's
/// result and replaces decimal argument 0.
struct Args<'a> {
    v: &'a [Value],
    first: Option<MyDecimal>,
    /// The receiver / `to` of a `dirty_<op>`; otherwise ops write into a fresh MyDecimal.
    recv: Option<MyDecimal>,
}

impl Args<'_> {
    fn str(&self, i: usize) -> &str {
        self.v[i].as_str().unwrap()
    }
    fn int(&self, i: usize) -> isize {
        self.v[i].as_i64().unwrap() as isize
    }
    /// Decimal argument: FromString, error ignored (as govec does).
    fn dec(&self, i: usize) -> MyDecimal {
        if let (0, Some(d)) = (i, self.first) {
            return d;
        }
        let mut d = MyDecimal::default();
        let _ = d.from_string(self.str(i).as_bytes());
        d
    }
    /// The object the op writes into (govec's `argReader.to`).
    fn to(&self) -> MyDecimal {
        self.recv.unwrap_or_default()
    }
}

/// Ops whose result can feed a chained op: (number of args, result, error).
const PRODUCERS: &[(&str, usize)] = &[
    ("from_int", 1),
    ("from_uint", 1),
    ("round", 3),
    ("add", 2),
    ("mul", 2),
    ("div", 3),
];

fn produce(op: &str, a: &Args<'_>) -> (MyDecimal, Result<(), Error>) {
    let mut to = a.to();
    let err = match op {
        "from_int" => {
            to.from_int(a.v[0].as_i64().unwrap());
            Ok(())
        }
        "from_uint" => {
            to.from_uint(a.v[0].as_u64().unwrap());
            Ok(())
        }
        "round" => a.dec(0).round(&mut to, a.int(1), mode(a.str(2))),
        "add" => decimal_add(&a.dec(0), &a.dec(1), &mut to),
        "mul" => decimal_mul(&a.dec(0), &a.dec(1), &mut to),
        "div" => decimal_div(&a.dec(0), &a.dec(1), &mut to, a.int(2)),
        _ => panic!("not a producer: {op}"),
    };
    (to, err)
}

fn exec(op: &str, args: &[Value]) -> (Value, Result<(), Error>) {
    if let Some((p, c)) = op.split_once("_then_") {
        let n = PRODUCERS.iter().find(|(name, _)| *name == p).unwrap().1;
        let (d, perr) = produce(
            p,
            &Args {
                v: &args[..n],
                first: None,
                recv: None,
            },
        );
        // Placeholder for the consumer's first decimal, replaced by `first`.
        let mut rest = vec![Value::Null];
        rest.extend_from_slice(&args[n..]);
        let (out, cerr) = exec_op(
            c,
            &Args {
                v: &rest,
                first: Some(d),
                recv: None,
            },
        );
        return (out, perr.and(cerr));
    }
    if let Some(inner) = op.strip_prefix("dirty_") {
        let recv = Args {
            v: args,
            first: None,
            recv: None,
        }
        .dec(0);
        return exec_op(
            inner,
            &Args {
                v: &args[1..],
                first: None,
                recv: Some(recv),
            },
        );
    }
    exec_op(
        op,
        &Args {
            v: args,
            first: None,
            recv: None,
        },
    )
}

fn exec_op(op: &str, a: &Args<'_>) -> (Value, Result<(), Error>) {
    let args = a.v;
    match op {
        "from_string" => {
            let mut d = a.to();
            let err = d.from_string(a.str(0).as_bytes());
            (dec_out(&d), err)
        }
        "from_int" | "from_uint" | "round" | "add" | "mul" => {
            let (d, err) = produce(op, a);
            (dec_out(&d), err)
        }
        "to_string" => {
            let d = a.dec(0);
            (
                json!({"s": String::from_utf8(d.to_string()).unwrap(), "str": d.string()}),
                Ok(()),
            )
        }
        "from_float64" => {
            let mut d = a.to();
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
        "sub" | "mod" => {
            let (x, y) = (a.dec(0), a.dec(1));
            let mut to = a.to();
            let err = match op {
                "sub" => decimal_sub(&x, &y, &mut to),
                _ => decimal_mod(&x, &y, &mut to),
            };
            (dec_out(&to), err)
        }
        "add_alias" | "sub_alias" | "mul_alias" | "mod_alias" | "div_alias" => {
            let (mut x, mut y) = (a.dec(0), a.dec(1));
            // div has fracIncr before the alias arg.
            let (frac_incr, alias) = match op {
                "div_alias" => (a.int(2), a.int(3)),
                _ => (0, a.int(2)),
            };
            let f = |x: &MyDecimal, y: &MyDecimal, to: &mut MyDecimal| match op {
                "add_alias" => decimal_add(x, y, to),
                "sub_alias" => decimal_sub(x, y, to),
                "mul_alias" => decimal_mul(x, y, to),
                "mod_alias" => decimal_mod(x, y, to),
                _ => decimal_div(x, y, to, frac_incr),
            };
            // M-ALIAS: Go's f(x, y, x) etc.; copy the aliased input, then write into it.
            match alias {
                1 => {
                    let x0 = x;
                    let err = f(&x0, &y, &mut x);
                    (dec_out(&x), err)
                }
                2 => {
                    let y0 = y;
                    let err = f(&x, &y0, &mut y);
                    (dec_out(&y), err)
                }
                3 => {
                    let x0 = x;
                    let err = f(&x0, &x0, &mut x);
                    (dec_out(&x), err)
                }
                _ => panic!("bad alias {alias}"),
            }
        }
        "div" => {
            let (x, y) = (a.dec(0), a.dec(1));
            let mut to = a.to();
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
            let mut d = a.to();
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
            let mut d = a.to();
            let err = d.unmarshal_json(a.str(0).as_bytes());
            (dec_out(&d), err)
        }
        "unmarshal_json_gs" => {
            let mut d = a.to();
            let err = d.unmarshal_json(&ungs(&a.v[0]));
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
            } else if op.starts_with("unmarshal_json")
                && e.to_string().as_bytes() != ungs(&w["msg"])
            {
                diffs.push(format!("err: got {e}, want {w}"));
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

/// A `vec.GoString`: a JSON string, or `{"hex": ...}` for bytes that are not UTF-8.
fn ungs(v: &Value) -> Vec<u8> {
    match v {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Object(m) => {
            let h = m["hex"].as_str().unwrap().as_bytes();
            h.chunks(2)
                .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
                .collect()
        }
        _ => panic!("not a GoString: {v}"),
    }
}

#[test]
fn govec_decimal() {
    let mut text = String::new();
    let path = vectors();
    let file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("{path}: {e} (no Go vector set for this target arch)"));
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "decimal");
    assert_eq!(header["tidb"], "v8.5.8");
    assert_eq!(header["goarch"], GO_TARGET.split('.').next().unwrap());
    if GO_TARGET == "amd64" {
        assert_eq!(header["goamd64"], "v1");
    }

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
        "{:<22} {:>7} {:>7} {:>7}",
        "op", "records", "passed", "panics"
    );
    for (op, (total, passed, panics)) in &stats {
        eprintln!("{op:<22} {total:>7} {passed:>7} {panics:>7}");
    }
    let passed: usize = stats.values().map(|s| s.1).sum();
    eprintln!("{:<22} {:>7} {:>7}", "total", n, passed);
    eprintln!(
        "wire round trips: {}",
        WIRE_CHECKS.load(std::sync::atomic::Ordering::Relaxed)
    );
    for f in failures.iter().take(40) {
        eprintln!("FAIL {f}");
    }
    assert!(
        failures.is_empty(),
        "{} of {n} records differ from Go",
        failures.len()
    );
}
