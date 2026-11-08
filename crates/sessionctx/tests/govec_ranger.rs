//! Replays the govec `ranger` vectors (tidb-rs-tracker `govec/domains/ranger.go`): the
//! ranger types of block D00.1 part 2a against Go TiDB v8.5.8.
//!
//! `GOVEC_RANGER_FILES` (comma-separated paths) replays extra-seed artifacts as well.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::contextutil::{
    ignore_warn_appender, new_plan_cache_tracker, new_range_fallback_handler,
};
use tidb_base::gostd::time::{fixed_zone, utc};
use tidb_sessionctx::errctx;
use tidb_sessionctx::exprctx::BuildContextRef;
use tidb_sessionctx::exprstatic::new_expr_context;
use tidb_sessionctx::ranger::context::RangerContext;
use tidb_sessionctx::ranger::{
    MutableRanges, Range, Ranges, full_int_range, full_not_null_range, full_range, has_full_range,
    ranges_intersect_ranges, ranges_mem_usage, ranges_subset,
};
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::collate;
use tidb_types::context::{Context as TypeContext, DefaultStmtNoWarningContext};
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::enum_type::Enum;
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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/ranger.jsonl.gz",
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

fn dec_of(s: &str) -> MyDecimal {
    let mut d = MyDecimal::default();
    let _ = d.from_string(s.as_bytes());
    d
}

/// d2DatumIn of the tracker (the codec / datum2 datum form).
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
        dm::KindFloat32 => d.set_float32(f64::from_bits(u(1)) as f32),
        dm::KindFloat64 => d.set_float64(f64::from_bits(u(1))),
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

fn range_in(v: &Value) -> Range {
    let list = |k: &str| -> Vec<Datum> {
        match &v[k] {
            Value::Array(a) => a.iter().map(datum_in).collect(),
            _ => Vec::new(),
        }
    };
    let colls = match &v["c"] {
        Value::Array(a) => a
            .iter()
            .map(|c| collate::get_collator(c.as_str().unwrap()))
            .collect(),
        _ => Vec::new(),
    };
    Range {
        low_val: list("l"),
        high_val: list("h"),
        collators: colls,
        low_exclude: v["le"].as_bool().unwrap(),
        high_exclude: v["he"].as_bool().unwrap(),
    }
}

fn ranges_in(v: &Value) -> Ranges {
    v.as_array().unwrap().iter().map(range_in).collect()
}

/// rTry: a Rust panic is `{"panic": true}`.
fn r_try(f: impl FnOnce() -> Value) -> Value {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| json!({ "panic": true }))
}

fn r_err(e: &Error) -> Value {
    json!({ "err": gs(&e.error()) })
}

fn r_str(r: Option<&Range>) -> Value {
    match r {
        None => Value::Null,
        Some(r) => gs(&r.string()),
    }
}

fn r_strs(rs: Option<&[Range]>) -> Value {
    match rs {
        None => Value::Null,
        Some(rs) => Value::Array(rs.iter().map(|r| r_str(Some(r))).collect()),
    }
}

fn ranger_ctx(tc: &TypeContext, null_as_point: bool) -> RangerContext {
    let tracker = Arc::new(new_plan_cache_tracker(None));
    let expr_ctx: BuildContextRef = new_expr_context(Vec::new());
    RangerContext {
        type_ctx: tc.clone(),
        err_ctx: errctx::StrictNoWarningContext.clone(),
        expr_ctx,
        range_fallback_handler: Arc::new(new_range_fallback_handler(
            Arc::clone(&tracker),
            ignore_warn_appender(),
        )),
        plan_cache_tracker: tracker,
        optimizer_fix_control: HashMap::new(),
        use_cache: false,
        in_prepared_plan_building: false,
        regard_null_as_point: null_as_point,
        opt_prefix_index_single_scan: false,
    }
}

fn exec(op: &str, p: &[Value]) -> Value {
    // As the generator: every record runs with new collations enabled.
    collate::set_new_collation_enabled_for_test(true);
    let tc = DefaultStmtNoWarningContext.clone();
    match op {
        "range" => {
            let r = range_in(&p[0]);
            let mut m = serde_json::Map::new();
            m.insert("s".into(), r_try(|| gs(&r.string())));
            m.insert("r_on".into(), r_try(|| gs(&r.redact("ON"))));
            m.insert("r_mk".into(), r_try(|| gs(&r.redact("MARKER"))));
            m.insert("w".into(), json!(r.width()));
            for (key, null) in [("p0", false), ("p1", true)] {
                let ctx = ranger_ctx(&tc, null);
                m.insert(key.into(), r_try(|| json!(r.is_point(&ctx))));
            }
            m.insert("pn".into(), r_try(|| json!(r.is_point_nullable(&tc))));
            m.insert("pnn".into(), r_try(|| json!(r.is_point_non_nullable(&tc))));
            m.insert("on".into(), r_try(|| json!(r.is_only_null())));
            m.insert("fu".into(), r_try(|| json!(r.is_full_range(true))));
            m.insert("fs".into(), r_try(|| json!(r.is_full_range(false))));
            m.insert("mem".into(), json!(r.mem_usage()));
            m.insert(
                "pel".into(),
                r_try(|| match r.prefix_equal_len(&tc) {
                    Ok(n) => json!(n),
                    Err(e) => r_err(&e),
                }),
            );
            m.insert(
                "cl".into(),
                r_try(|| {
                    let c = r.clone_range();
                    json!({ "s": r_str(Some(&c)), "eq": c.equal(&r), "mem": c.mem_usage() })
                }),
            );
            Value::Object(m)
        }
        "encode" => {
            let r = range_in(&p[0]);
            let loc = if p[1].is_null() {
                utc()
            } else {
                fixed_zone("Z", p[1].as_i64().unwrap() as i32)
            };
            let ec = if p[2].as_str() == Some("ignore") {
                errctx::new_context_with_levels([errctx::LevelIgnore; 7], ignore_warn_appender())
            } else {
                errctx::StrictNoWarningContext.clone()
            };
            r_try(|| match r.encode(&ec, &loc, Vec::new(), Vec::new()) {
                Ok((lo, hi)) => json!({ "lo": hex(&lo), "hi": hex(&hi) }),
                Err(e) => r_err(&e),
            })
        }
        "pair" => {
            let (r1, r2) = (range_in(&p[0]), range_in(&p[1]));
            let mut m = serde_json::Map::new();
            m.insert("eq".into(), r_try(|| json!(r1.equal(&r2))));
            m.insert("sub".into(), r_try(|| json!(r1.subset(&tc, &r2))));
            m.insert(
                "isect".into(),
                r_try(|| match r1.intersect_range(&tc, &r2) {
                    Ok(x) => r_str(x.as_ref()),
                    Err(e) => r_err(&e),
                }),
            );
            Value::Object(m)
        }
        "ranges" => {
            let (rs1, rs2) = (ranges_in(&p[0]), ranges_in(&p[1]));
            let mut m = serde_json::Map::new();
            m.insert(
                "sub".into(),
                r_try(|| json!(ranges_subset(&rs1, &tc, &rs2))),
            );
            m.insert(
                "isect".into(),
                r_try(|| r_strs(ranges_intersect_ranges(&rs1, &tc, &rs2).as_deref())),
            );
            m.insert("hf_u".into(), r_try(|| json!(has_full_range(&rs1, true))));
            m.insert("hf_s".into(), r_try(|| json!(has_full_range(&rs1, false))));
            m.insert("mem".into(), json!(ranges_mem_usage(&rs1)));
            m.insert(
                "cfpc".into(),
                r_try(|| match rs1.clone_for_plan_cache() {
                    None => Value::Null,
                    Some(c) => r_strs(Some(&c.range())),
                }),
            );
            m.insert("rng".into(), r_strs(Some(&rs1.range())));
            m.insert("rebuild".into(), json!(true));
            Value::Object(m)
        }
        "full" => {
            let mut m = serde_json::Map::new();
            for (k, rs) in [
                ("int_s", full_int_range(false)),
                ("int_u", full_int_range(true)),
                ("full", full_range()),
                ("nn", full_not_null_range()),
            ] {
                m.insert(
                    k.into(),
                    json!({
                        "s": r_strs(Some(&rs)),
                        "mem": ranges_mem_usage(&rs),
                        "fu": has_full_range(&rs, true),
                        "fs": has_full_range(&rs, false),
                    }),
                );
            }
            Value::Object(m)
        }
        _ => panic!("ranger: unknown op {op}"),
    }
}

fn replay(path: &str) {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "ranger");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let args = rec["args"].as_array().cloned().unwrap_or_default();
        let got = exec(rec["op"].as_str().unwrap(), &args);
        if got != rec["out"] {
            fails.push(format!("{line}\n    got {got}"));
        }
    }
    std::panic::set_hook(prev);
    let shown: Vec<_> = fails.iter().take(30).map(|s| s.as_str()).collect();
    assert!(
        fails.is_empty(),
        "{path}: {} of {n} records differ:\n{}",
        fails.len(),
        shown.join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

#[test]
fn govec_ranger() {
    replay(&vectors());
    if let Ok(files) = std::env::var("GOVEC_RANGER_FILES") {
        for f in files.split(',').filter(|f| !f.is_empty()) {
            replay(f);
        }
    }
}
