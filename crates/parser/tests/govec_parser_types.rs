//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/parser_types.jsonl.gz`
//! (tracker `govec`, domain "parser_types", block D02.3) against the Rust port. The ops and
//! their arguments are described in the tracker's `govec/domains/parsertypes.go`.
//!
//! Errors: the message must be Go's, except JSON *syntax* errors (`*json.SyntaxError` texts
//! are not ported): there only "an error" is required. `err.name == "panic"` records must
//! panic in Rust as well.

use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::Ordering;

use tidb_parser::auth::{self, RoleIdentity, UserIdentity};
use tidb_parser::types::{self, EvalType, FieldType, IHasher};
use tidb_parser::{ast_ddl, duration, model};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A Go string as govec writes it (`vec.GoString`).
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({"hex": b.iter().map(|x| format!("{x:02x}")).collect::<String>()}),
    }
}

fn gs_bytes(v: &Value) -> Vec<u8> {
    match v.as_str() {
        Some(s) => s.as_bytes().to_vec(),
        None => unhex(v["hex"].as_str().unwrap()),
    }
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap()
}

/// `ftSpec.build()`.
fn build(spec: &Value) -> FieldType {
    let mut ft = FieldType::new(i(&spec["tp"]) as u8);
    ft.set_flag(spec["flag"].as_u64().unwrap() as usize);
    ft.set_flen(i(&spec["flen"]) as isize);
    ft.set_decimal(i(&spec["decimal"]) as isize);
    ft.set_charset(s(&spec["charset"]));
    ft.set_collate(s(&spec["collate"]));
    if let Some(es) = spec["elems"].as_array() {
        let es: Vec<Vec<u8>> = es.iter().map(gs_bytes).collect();
        ft.set_elems(Some(es.clone()));
        for b in spec
            .get("bin")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let k = b.as_u64().unwrap() as usize;
            ft.set_elem_with_is_binary_lit(k, es[k].clone(), true);
        }
    }
    ft.set_array(spec["array"].as_bool().unwrap());
    ft
}

fn fields(ft: &FieldType) -> Value {
    let n = ft.get_elems().map_or(0, <[Vec<u8>]>::len);
    let bins: Vec<bool> = (0..n).map(|k| ft.get_elem_is_binary_lit(k)).collect();
    json!({
        "tp": ft.get_type(), "flag": ft.get_flag(), "flen": ft.get_flen(), "decimal": ft.get_decimal(),
        "charset": ft.get_charset(), "collate": ft.get_collate(),
        "elems": ft.get_elems().map(|es| es.iter().map(|e| gs(e)).collect::<Vec<_>>()),
        "bin": bins, "array": ft.is_array(), "json": ft.marshal_json(),
    })
}

fn ft_out(ft: &FieldType) -> Value {
    let mut out = fields(ft);
    let mut restore = Vec::new();
    ft.restore(&mut restore).unwrap();
    let mut cast = Vec::new();
    ft.restore_as_cast_type(&mut cast, false);
    let mut cast_cs = Vec::new();
    ft.restore_as_cast_type(&mut cast_cs, true);
    let m = out.as_object_mut().unwrap();
    m.insert("compact".into(), json!(ft.compact_str()));
    m.insert("info".into(), json!(ft.info_schema_str()));
    m.insert("string".into(), json!(ft.to_string()));
    m.insert("restore".into(), gs(&restore));
    m.insert("cast".into(), gs(&cast));
    m.insert("cast_cs".into(), gs(&cast_cs));
    m.insert("eval".into(), json!(ft.eval_type().0));
    m.insert("hybrid".into(), json!(ft.hybrid()));
    m.insert("varlen".into(), json!(ft.is_var_length_type()));
    m.insert("dec_valid".into(), json!(ft.is_decimal_valid()));
    m.insert("has_cs".into(), json!(types::has_charset(ft)));
    out
}

#[derive(Default)]
struct RecHasher(Vec<String>);

impl IHasher for RecHasher {
    fn hash_bool(&mut self, v: bool) {
        self.0.push(format!("bool:{v}"));
    }
    fn hash_int(&mut self, v: isize) {
        self.0.push(format!("int:{v}"));
    }
    fn hash_int64(&mut self, v: i64) {
        self.0.push(format!("int64:{v}"));
    }
    fn hash_uint64(&mut self, v: u64) {
        self.0.push(format!("uint64:{v}"));
    }
    fn hash_float64(&mut self, v: f64) {
        self.0.push(format!("float64:{v}"));
    }
    fn hash_rune(&mut self, v: char) {
        self.0.push(format!("rune:{}", v as u32));
    }
    fn hash_string(&mut self, v: &[u8]) {
        let h: String = v.iter().map(|b| format!("{b:02x}")).collect();
        self.0.push(format!("string:{h}"));
    }
    fn hash_byte(&mut self, v: u8) {
        self.0.push(format!("byte:{v}"));
    }
    fn hash_bytes(&mut self, v: &[u8]) {
        let h: String = v.iter().map(|b| format!("{b:02x}")).collect();
        self.0.push(format!("bytes:{h}"));
    }
    fn reset(&mut self) {
        self.0.clear();
    }
    fn sum64(&self) -> u64 {
        0
    }
}

fn model_str(kind: &str, v: i64) -> &'static str {
    match kind {
        "TableLockType" => model::TableLockType(v as u8).to_string(),
        "ViewAlgorithm" => model::ViewAlgorithm(v as isize).to_string(),
        "ViewSecurity" => model::ViewSecurity(v as isize).to_string(),
        "ViewCheckOption" => model::ViewCheckOption(v as isize).to_string(),
        "PartitionType" => model::PartitionType(v as isize).to_string(),
        "PrimaryKeyType" => model::PrimaryKeyType(v as i8).to_string(),
        "IndexType" => model::IndexType(v as isize).to_string(),
        "ReferOptionType" => model::ReferOptionType(v as isize).to_string(),
        "RunawayWatchType" => model::RunawayWatchType(v as i32).to_string(),
        "RunawayActionType" => model::RunawayActionType(v as i32).to_string(),
        "ColumnChoice" => model::ColumnChoice(v as u8).to_string(),
        _ => panic!("unknown kind {kind}"),
    }
}

fn user(v: &Value) -> UserIdentity {
    UserIdentity {
        username: s(&v["Username"]).to_owned(),
        hostname: s(&v["Hostname"]).to_owned(),
        current_user: v["CurrentUser"].as_bool().unwrap(),
        auth_username: s(&v["AuthUsername"]).to_owned(),
        auth_hostname: s(&v["AuthHostname"]).to_owned(),
        auth_plugin: s(&v["AuthPlugin"]).to_owned(),
    }
}

type Res = (Value, Option<String>);

fn ok(v: Value) -> Res {
    (v, None)
}

fn exec(op: &str, a: &[Value]) -> Res {
    match op {
        "ft" => {
            types::TiDBStrictIntegerDisplayWidth.store(a[1].as_bool().unwrap(), Ordering::Relaxed);
            let out = ft_out(&build(&a[0]));
            types::TiDBStrictIntegerDisplayWidth.store(false, Ordering::Relaxed);
            ok(out)
        }
        "ft_storage" => ok(json!(build(&a[0]).storage_length())),
        "ft_unmarshal" => {
            let mut ft = FieldType::default();
            match ft.unmarshal_json(&gs_bytes(&a[0])) {
                Ok(()) => ok(fields(&ft)),
                Err(e) => (Value::Null, Some(String::from_utf8(e.error()).unwrap())),
            }
        }
        "ft_unmarshal_dirty" => {
            let mut ft = build(&a[0]);
            let err = ft.unmarshal_json(&gs_bytes(&a[1])).err();
            (
                fields(&ft),
                err.map(|e| String::from_utf8(e.error()).unwrap()),
            )
        }
        "ft_equal" => {
            let (x, y) = (build(&a[0]), build(&a[1]));
            ok(
                json!({"equal": x.equal(&y), "partial": x.partial_equal(&y, false),
                "partial_unsafe": x.partial_equal(&y, true), "equals": x.equals(Some(&y)),
                "equals_val": x.equals(Some(&y)), "equals_nil": x.equals(None)}),
            )
        }
        "ft_mutate" => {
            let mut ft = build(&a[0]);
            for st in a[1].as_array().into_iter().flatten() {
                let st = st.as_array().unwrap();
                let n = |k: usize| i(&st[k]);
                match s(&st[0]) {
                    "set_type" => ft.set_type(n(1) as u8),
                    "set_flag" => ft.set_flag(n(1) as usize),
                    "add_flag" => ft.add_flag(n(1) as usize),
                    "and_flag" => ft.and_flag(n(1) as usize),
                    "toggle_flag" => ft.toggle_flag(n(1) as usize),
                    "del_flag" => ft.del_flag(n(1) as usize),
                    "set_flen" => ft.set_flen(n(1) as isize),
                    "set_decimal" => ft.set_decimal(n(1) as isize),
                    "set_flen_limit" => ft.set_flen_under_limit(n(1) as isize),
                    "set_decimal_limit" => ft.set_decimal_under_limit(n(1) as isize),
                    "set_array" => ft.set_array(n(1) != 0),
                    "init" => ft.init(n(1) as u8),
                    "set_elem" => ft.set_elem(n(1) as usize, gs_bytes(&st[2])),
                    "set_elem_bin" => ft.set_elem_with_is_binary_lit(
                        n(1) as usize,
                        gs_bytes(&st[2]),
                        st[3].as_bool().unwrap(),
                    ),
                    "clean_bin" => ft.clean_elem_is_binary_lit(),
                    "update_limit" => {
                        let mut old = FieldType::new(n(3) as u8);
                        old.set_flen(n(1) as isize);
                        old.set_decimal(n(2) as isize);
                        ft.update_flen_and_decimal_under_limit(&old, n(4) as isize, n(5) as isize);
                    }
                    other => panic!("unknown step {other}"),
                }
            }
            ok(fields(&ft))
        }
        "ft_copy" => {
            let ft = build(&a[0]);
            ok(
                json!({"deep": fields(&ft.deep_copy()), "clone": fields(&ft.clone()),
                "array_type": fields(&ft.array_type())}),
            )
        }
        "ft_hash" => {
            let mut h = RecHasher::default();
            build(&a[0]).hash64(&mut h);
            ok(json!(h.0))
        }
        "ft_mem" => ok(json!(build(&a[0]).memory_usage())),
        "type_str" => {
            let tp = i(&a[0]) as u8;
            ok(
                json!({"str": types::type_str(tp), "to_str": types::type_to_str(tp, s(&a[1])),
                "blob": types::is_type_blob(tp), "char": types::is_type_char(tp),
                "vector": types::is_type_vector(tp)}),
            )
        }
        "str_to_type" => ok(json!(types::str_to_type(s(&a[0])))),
        "eval_type" => {
            let et = EvalType(i(&a[0]) as u8);
            ok(
                json!({"string_kind": et.is_string_kind(), "vector_kind": et.is_vector_kind(),
                "string": et.to_string()}),
            )
        }
        "cistr" => {
            let c = model::CIStr::new(s(&a[0]));
            ok(
                json!({"o": c.o, "l": c.l, "string": c.to_string(), "mem": c.memory_usage(),
                "json": c.marshal_json()}),
            )
        }
        "cistr_unmarshal" => {
            let mut c = model::CIStr {
                o: s(&a[0]).to_owned(),
                l: s(&a[1]).to_owned(),
            };
            let err = c.unmarshal_json(&gs_bytes(&a[2])).err();
            (
                json!({"o": c.o, "l": c.l}),
                err.map(|e| String::from_utf8(e.error()).unwrap()),
            )
        }
        "model_str" => ok(json!(model_str(s(&a[0]), i(&a[1])))),
        "priority" => ok(json!(model::priority_value_to_name(a[0].as_u64().unwrap()))),
        "user" => {
            let u = user(&a[0]);
            let mut r = String::new();
            u.restore(&mut r);
            let (un, hn) = auth::get_user_and_host_name(Some(&u));
            ok(
                json!({"restore": r, "string": u.to_string(), "login": u.login_string(),
                "get": [un, hn]}),
            )
        }
        "user_nil" => {
            let (un, hn) = auth::get_user_and_host_name(None);
            // A nil *UserIdentity prints "" for String and LoginString.
            ok(json!([un, hn, "", ""]))
        }
        "role" => {
            let r = RoleIdentity {
                username: s(&a[0]).to_owned(),
                hostname: s(&a[1]).to_owned(),
            };
            let mut out = String::new();
            r.restore(&mut out);
            ok(json!({"restore": out, "string": r.to_string()}))
        }
        "duration" => match duration::parse_duration(s(&a[0])) {
            Ok(d) => ok(json!(d)),
            Err(e) => (json!(0), Some(String::from_utf8(e.error()).unwrap())),
        },
        "affinity" => {
            let (s2, b) = ast_ddl::normalize_table_affinity_level(s(&a[0]));
            ok(json!([s2, b]))
        }
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_parser_types() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/parser_types.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut text = String::new();
    flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap())
        .read_to_string(&mut text)
        .unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "parser_types");
    std::panic::set_hook(Box::new(|_| {}));
    let (mut n, mut bad) = (0, Vec::new());
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = s(&rec["op"]);
        let args = rec["args"].as_array().unwrap();
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
        let err = &rec["err"];
        let ok = match got {
            Err(_) => err["name"] == "panic",
            Ok(_) if err["name"] == "panic" => false,
            Ok((out, e)) => {
                let want_msg =
                    (!err.is_null()).then(|| String::from_utf8(gs_bytes(&err["msg"])).unwrap());
                let err_ok = match (&want_msg, &e) {
                    (None, None) => true,
                    (Some(w), Some(g)) => {
                        // JSON syntax error texts are not ported.
                        w == g
                            || (!w.starts_with("json: cannot unmarshal")
                                && !g.starts_with("json: cannot unmarshal")
                                && (op.contains("unmarshal")))
                    }
                    _ => false,
                };
                let out_ok = (rec["out"].is_null() && e.is_some()) || rec["out"] == out;
                err_ok && out_ok
            }
        };
        if !ok && bad.len() < 20 {
            bad.push(line.to_owned());
        }
        if !ok && bad.len() >= 20 {
            break;
        }
    }
    let _ = std::panic::take_hook();
    assert!(
        bad.is_empty(),
        "{} of {n} records differ, e.g.:\n{}",
        bad.len(),
        bad.join("\n")
    );
    assert_eq!(n, header["records"].as_u64().unwrap() as usize);
}
