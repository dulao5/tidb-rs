//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/base.jsonl.gz`
//! (tracker `govec`, domain "base", block D00.2) against the Rust port.
//!
//! Two kinds of records (tracker `govec/domains/base.go`):
//! * `table`: a full Go table (`mysql.MySQLErrName`, the error registry, the prototypes ...)
//!   as Go has it at run time; the Rust table (generated from the Go source by
//!   `govec/cmd/rsgen`) must be equal.
//! * calls: error formatting (`fmt.Sprintf` with dynamic arguments, redaction), `terror` /
//!   `dbterror` classes, SQL modes, privileges, charset/type helpers, `FORMAT` en_US.
//!
//! The domain has no floating-point arithmetic that gc could fuse (`M-FMA`); the target
//! split only follows the vector layout (`x86_64` -> `amd64`, `aarch64` -> `arm64`).
//! `err.name == "panic"` records must panic in Rust as well.

use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use tidb_base::errors::{self, Error};
use tidb_base::gofmt::{self, Arg};
use tidb_base::gostd::{is_digit, is_one_of};
use tidb_base::mysql::{self, PriorityEnum, PrivilegeType, RestoreKeyWord, SQLError, SQLMode};
use tidb_base::{dbterror, errno, kv, terror};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/base.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

/// A Go string as govec writes it (`vec.GoString`): a JSON string, or {"hex"} if not UTF-8.
fn gs(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => json!(s),
        Err(_) => json!({"hex": b.iter().map(|x| format!("{x:02x}")).collect::<String>()}),
    }
}

/// The bytes of a `vec.GoString` value.
fn gs_bytes(v: &Value) -> Vec<u8> {
    match v.as_str() {
        Some(s) => s.as_bytes().to_vec(),
        None => unhex(v["hex"].as_str().unwrap()),
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// A typed argument {"t", "v"} as a Go `interface{}` value.
fn arg(v: &Value) -> Arg {
    let t = v["t"].as_str().unwrap();
    let x = &v["v"];
    let s = || match v.get("hex") {
        Some(h) => unhex(h.as_str().unwrap()),
        None => x.as_str().unwrap().as_bytes().to_vec(),
    };
    match t {
        "string" => Arg::go_string(s()),
        "int" => Arg::Int(x.as_i64().unwrap(), "int"),
        "int8" | "int16" | "int32" | "int64" => Arg::Int(x.as_i64().unwrap(), leak(t.to_owned())),
        "uint" | "uint8" | "uint16" | "uint32" | "uint64" => {
            Arg::Uint(x.as_u64().unwrap(), leak(t.to_owned()))
        }
        "float64" => Arg::Float(x.as_str().unwrap().parse().unwrap(), 64),
        "float32" => Arg::Float(x.as_str().unwrap().parse::<f32>().unwrap() as f64, 32),
        "bool" => Arg::Bool(x.as_bool().unwrap()),
        "nil" => Arg::Nil,
        "bytes" => Arg::Bytes(unhex(x.as_str().unwrap())),
        "error" => Arg::Error(s()),
        _ => panic!("unknown typed arg {t}"),
    }
}

fn args(v: &Value) -> Vec<Arg> {
    v.as_array().unwrap().iter().map(arg).collect()
}

fn with_redact<R>(mode: &str, f: impl FnOnce() -> R) -> R {
    errors::redact_log_enabled_store(mode);
    let r = catch_unwind(AssertUnwindSafe(f));
    errors::redact_log_enabled_store(errors::RedactLogDisable);
    r.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

fn terr(proto: &Error, generated: &Error) -> Value {
    json!({
        "code": proto.code(), "rfc": proto.rfc_code(), "template": gs(proto.message_template()),
        "redact": proto.redact_args_pos(), "msg": gs(&generated.get_msg()), "error": gs(&generated.error()),
    })
}

fn sql_err(e: &SQLError) -> Value {
    json!({"code": e.code, "state": e.state, "message": gs(&e.message), "error": gs(&e.error())})
}

fn pairs<K: serde::Serialize, V>(
    it: impl Iterator<Item = (K, V)>,
    f: impl Fn(V) -> Value,
) -> Value {
    Value::Array(it.map(|(k, v)| json!([k, f(v)])).collect())
}

/// All prototypes by "<go file>:<name>".
static PROTOS: LazyLock<Vec<(String, &'static LazyLock<Error>)>> = LazyLock::new(|| {
    let mut v = Vec::new();
    for (file, all) in [
        ("pkg/util/dbterror/ddl_terror.go", dbterror::ALL),
        (
            "pkg/util/dbterror/exeerrors/errors.go",
            dbterror::exeerrors::ALL,
        ),
        (
            "pkg/util/dbterror/plannererrors/planner_terror.go",
            dbterror::plannererrors::ALL,
        ),
    ] {
        for (name, e) in all {
            v.push((format!("{file}:{name}"), *e));
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
});

fn proto(key: &str) -> &'static Error {
    let (_, e) = PROTOS
        .iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| panic!("no prototype {key}"));
    e
}

fn priv_list(name: &str) -> &'static mysql::Privileges {
    match name {
        "AllGlobalPrivs" => mysql::AllGlobalPrivs,
        "AllDBPrivs" => mysql::AllDBPrivs,
        "AllTablePrivs" => mysql::AllTablePrivs,
        "AllColumnPrivs" => mysql::AllColumnPrivs,
        "StaticGlobalOnlyPrivs" => mysql::StaticGlobalOnlyPrivs,
        _ => panic!("no list {name}"),
    }
}

fn table(name: &str) -> Value {
    let msg = |m: &mysql::ErrMessage| json!([m.raw, m.redact_arg_pos]);
    let privs = |l: &[PrivilegeType]| json!(l.iter().map(|p| p.0).collect::<Vec<_>>());
    match name {
        "mysql.MySQLErrName" => pairs(mysql::MySQLErrName.iter(), msg),
        "errno.MySQLErrName" => pairs(errno::MySQLErrName.iter(), msg),
        "mysql.MySQLState" => pairs(mysql::MySQLState.iter(), |v| json!(v)),
        "mysql.CharsetIDs" => pairs(mysql::CharsetIDs.iter(), |v| json!(v)),
        "mysql.Collations" => pairs(mysql::Collations.iter(), |v| json!(v)),
        "mysql.CollationNames" => pairs(mysql::CollationNames.iter(), |v| json!(v)),
        "mysql.Command2Str" => pairs(mysql::Command2Str.iter(), |v| json!(v)),
        "mysql.DefaultLengthOfMysqlTypes" => {
            pairs(mysql::DefaultLengthOfMysqlTypes.iter(), |v| json!(v))
        }
        "mysql.DefaultLengthOfTimeFraction" => {
            pairs(mysql::DefaultLengthOfTimeFraction.iter(), |v| json!(v))
        }
        "mysql.Str2SQLMode" => pairs(mysql::Str2SQLMode.iter(), |v| json!(v.0)),
        "mysql.CombinationSQLMode" => pairs(mysql::CombinationSQLMode.iter(), |v| json!(v)),
        "mysql.Priority2Str" => pairs(mysql::Priority2Str.iter().map(|(k, v)| (k.0, v)), |v| {
            json!(v)
        }),
        "mysql.DefaultAuthPlugins" => json!(mysql::DefaultAuthPlugins),
        "mysql.Priv2Str" => pairs(mysql::Priv2Str.iter().map(|(k, v)| (k.0, v)), |v| json!(v)),
        "mysql.Priv2SetStr" => pairs(mysql::Priv2SetStr.iter().map(|(k, v)| (k.0, v)), |v| {
            json!(v)
        }),
        "mysql.Priv2UserCol" => pairs(mysql::Priv2UserCol.iter().map(|(k, v)| (k.0, v)), |v| {
            json!(v)
        }),
        "mysql.SetStr2Priv" => pairs(mysql::SetStr2Priv.iter(), |v| json!(v.0)),
        "mysql.Col2PrivType" => pairs(mysql::Col2PrivType.iter(), |v| json!(v.0)),
        "mysql.AllGlobalPrivs" => privs(mysql::AllGlobalPrivs),
        "mysql.AllDBPrivs" => privs(mysql::AllDBPrivs),
        "mysql.AllTablePrivs" => privs(mysql::AllTablePrivs),
        "mysql.AllColumnPrivs" => privs(mysql::AllColumnPrivs),
        "mysql.StaticGlobalOnlyPrivs" => privs(mysql::StaticGlobalOnlyPrivs),
        "terror.ErrClassToMySQLCodes" => pairs(
            terror::ErrClassToMySQLCodes.iter().map(|(k, v)| (k.0, v)),
            |v| json!(v),
        ),
        "terror.classes" => Value::Array(
            (-1..=30)
                .map(|c| json!([c, terror::ErrClass(c).string()]))
                .collect(),
        ),
        "dbterror.prototypes" => {
            Value::Array(PROTOS.iter().map(|(k, e)| json!([k, terr(e, e)])).collect())
        }
        _ => panic!("unknown table {name}"),
    }
}

struct KeyWords(String);

impl RestoreKeyWord for KeyWords {
    fn write_key_word(&mut self, key_word: &str) {
        // format.DefaultRestoreFlags includes RestoreKeyWordUppercase.
        self.0.push_str(&key_word.to_uppercase());
    }
}

fn mk_err(v: &Value) -> Option<Error> {
    match v["kind"].as_str().unwrap() {
        "terr" => Some(
            terror::ErrClass(v["class"].as_i64().unwrap() as isize).synthesize(
                v["code"].as_i64().unwrap() as isize,
                v["msg"].as_str().unwrap().to_owned(),
            ),
        ),
        "plain" => Some(errors::new(v["msg"].as_str().unwrap().to_owned())),
        _ => None,
    }
}

/// Runs one record; Ok(out) or Err(error message) like Go's (out, err).
fn exec(op: &str, a: &[Value]) -> Result<Value, Value> {
    let s = |i: usize| a[i].as_str().unwrap();
    let i = |i: usize| a[i].as_i64().unwrap();
    let u = |i: usize| a[i].as_u64().unwrap();
    Ok(match op {
        "table" => table(s(0)),
        "sprintf" | "sprintf_rand" => gs(&gofmt::sprintf(gs_bytes(&a[0]), &args(&a[1]))),
        "sprint" => gs(&gofmt::sprint(&args(&a[0]))),
        "new_err" => sql_err(&with_redact(s(2), || {
            mysql::new_err(u(0) as u16, args(&a[1]))
        })),
        "new_errf" => {
            let pos: Vec<usize> = a[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_u64().unwrap() as usize)
                .collect();
            sql_err(&with_redact(s(4), || {
                mysql::new_errf(u(0) as u16, s(1), &pos, args(&a[3]))
            }))
        }
        "terror_new_std" | "dbterror_new_std" => {
            let class = terror::ErrClass(i(0) as isize);
            let p = if op == "terror_new_std" {
                class.new_std(i(1) as isize)
            } else {
                dbterror::ErrClass(class).new_std(i(1) as isize)
            };
            with_redact(s(3), || terr(&p, &p.gen_with_stack_by_args(args(&a[2]))))
        }
        "prototype" => {
            let p = proto(s(0));
            with_redact(s(2), || terr(p, &p.gen_with_stack_by_args(args(&a[1]))))
        }
        "synthesize" => {
            let e = terror::ErrClass(i(0) as isize).synthesize(i(1) as isize, gs_bytes(&a[2]));
            terr(&e, &e)
        }
        "to_sql_error" => {
            let e = terror::ErrClass(i(0) as isize).synthesize(i(1) as isize, gs_bytes(&a[2]));
            sql_err(&terror::to_sql_error(&e))
        }
        "err_class" => {
            let class = terror::ErrClass(i(0) as isize);
            let e = errors::normalize(
                "m",
                [
                    errors::rfc_code_text(format!("{}:{}", s(1), i(2))),
                    errors::mysql_error_code(i(2) as isize),
                ],
            );
            json!([
                class.equal_class(Some(&e)),
                class.not_equal_class(Some(&e)),
                terror::get_err_class(&e).0
            ])
        }
        "error_equal" => {
            let (x, y) = (mk_err(&a[0]), mk_err(&a[1]));
            let (x, y) = (x.as_ref(), y.as_ref());
            json!([
                terror::error_equal(x, y),
                terror::error_not_equal(x, y),
                errors::error_equal(x, y)
            ])
        }
        "format_sql_mode_str" => json!(mysql::format_sql_mode_str(s(0))),
        "get_sql_mode" => {
            let (m, e) = mysql::get_sql_mode(s(0));
            json!([m.0, e.err().map(|e| sql_err(&e))])
        }
        "sql_mode_has" => {
            let m = SQLMode(i(0));
            json!([
                m.has_no_zero_date_mode(),
                m.has_no_zero_in_date_mode(),
                m.has_error_for_division_by_zero_mode(),
                m.has_only_full_group_by(),
                m.has_strict_mode(),
                m.has_pipes_as_concat_mode(),
                m.has_no_unsigned_subtraction_mode(),
                m.has_high_not_precedence_mode(),
                m.has_ansi_quotes_mode(),
                m.has_real_as_float_mode(),
                m.has_pad_char_to_full_length_mode(),
                m.has_no_backslash_escapes_mode(),
                m.has_ignore_space_mode(),
                m.has_no_auto_create_user_mode(),
                m.has_allow_invalid_dates_mode()
            ])
        }
        "sql_mode_set" => {
            let (x, y) = (SQLMode(i(0)), SQLMode(i(1)));
            json!([mysql::del_sql_mode(x, y).0, mysql::set_sql_mode(x, y).0])
        }
        "str2priority" => json!(mysql::str2_priority(s(0)).0),
        "priority_restore" => {
            let mut k = KeyWords(String::new());
            PriorityEnum(i(0) as isize)
                .restore(&mut k)
                .map_err(|e| gs(&e.error()))?;
            json!(k.0)
        }
        "has_cursor_exists_flag" => json!(mysql::has_cursor_exists_flag(u(0) as u16)),
        "type_flags" => {
            let f = u(0) as usize;
            json!([
                mysql::has_drop_column_with_index_flag(f),
                mysql::has_not_null_flag(f),
                mysql::has_no_default_value_flag(f),
                mysql::has_auto_increment_flag(f),
                mysql::has_unsigned_flag(f),
                mysql::has_zerofill_flag(f),
                mysql::has_binary_flag(f),
                mysql::has_pri_key_flag(f),
                mysql::has_uni_key_flag(f),
                mysql::has_multiple_key_flag(f),
                mysql::has_timestamp_flag(f),
                mysql::has_on_update_now_flag(f),
                mysql::has_parse_to_json_flag(f),
                mysql::has_is_boolean_flag(f),
                mysql::has_prevent_null_insert_flag(f),
                mysql::has_enum_set_as_int_flag(f)
            ])
        }
        "charset" => json!([
            mysql::charset_name_to_id(s(0)),
            mysql::is_utf8_charset(s(0))
        ]),
        "unicode" => match char::from_u32(i(0) as u32) {
            Some(c) => json!([is_one_of(&mysql::RangeGraph, c), is_digit(c)]),
            None => json!([false, false]), // surrogates: in no table
        },
        "new_priv" => {
            let (p1, ok1) = mysql::new_priv_from_column(s(0));
            let (p2, ok2) = mysql::new_priv_from_set_enum(s(0));
            json!([p1.0, ok1, p2.0, ok2])
        }
        "priv_strings" => {
            let p = PrivilegeType(u(0));
            json!([p.string(), p.column_string(), p.set_string()])
        }
        "privileges_has" => json!(mysql::privileges_has(priv_list(s(0)), PrivilegeType(u(1)))),
        "type_defaults" => {
            let tp = u(0) as u8;
            let (f1, d1) = mysql::get_default_field_length_and_decimal(tp);
            let (f2, d2) = mysql::get_default_field_length_and_decimal_for_cast(tp);
            json!([mysql::is_integer_type(tp), f1, d1, f2, d2])
        }
        "auth_clear_text" => json!(mysql::is_auth_plugin_clear_text(s(0))),
        "format_locale" => {
            let f = mysql::get_locale_format_function(s(0));
            gs(&f(&gs_bytes(&a[1]), &gs_bytes(&a[2])).map_err(|e| gs(&e.error()))?)
        }
        "errno_stats" => {
            errno::flush_stats();
            for step in a[0].as_array().unwrap() {
                let code = step[1].as_u64().unwrap() as u16;
                let (user, host) = (step[2].as_str().unwrap(), step[3].as_str().unwrap());
                if step[0] == "error" {
                    errno::increment_error(code, user, host);
                } else {
                    errno::increment_warning(code, user, host);
                }
            }
            let mut out = Vec::new();
            let mut add = |key: String, m: std::collections::HashMap<u16, errno::ErrorSummary>| {
                let mut codes: Vec<_> = m.keys().copied().collect();
                codes.sort();
                for c in codes {
                    let e = &m[&c];
                    out.push(json!({"key": key, "code": c, "errors": e.error_count,
                        "warnings": e.warning_count, "last_seen_set": e.last_seen.is_some()}));
                }
            };
            add("global".into(), errno::global_stats());
            for (kind, m) in [("user", errno::user_stats()), ("host", errno::host_stats())] {
                let mut ks: Vec<_> = m.keys().cloned().collect();
                ks.sort();
                for k in ks {
                    add(format!("{kind}:{k}"), m[&k].clone());
                }
            }
            Value::Array(out)
        }
        "store_type" => json!(kv::StoreType(u(0) as u8).name()),
        _ => panic!("unknown op {op}"),
    })
}

#[test]
fn govec_base() {
    let mut gz = flate2::read::GzDecoder::new(
        std::fs::File::open(vectors()).unwrap_or_else(|e| panic!("{}: {e}", vectors())),
    );
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "base");
    let (mut n, mut failures) = (0, Vec::new());
    std::panic::set_hook(Box::new(|_| {}));
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let op = rec["op"].as_str().unwrap();
        let a = rec["args"].as_array().unwrap();
        let got = catch_unwind(AssertUnwindSafe(|| exec(op, a)));
        let ok = match (&got, rec["err"].as_object()) {
            (Err(_), Some(e)) => e["name"] == "panic",
            (Err(_), None) => false,
            (Ok(Err(msg)), Some(e)) => e["name"] != "panic" && e["msg"] == *msg,
            (Ok(Err(_)), None) => false,
            (Ok(Ok(_)), Some(e)) if e["name"] != "panic" => false,
            (Ok(Ok(_)), Some(_)) => false,
            (Ok(Ok(v)), None) => *v == rec["out"],
        };
        if !ok {
            let got = match got {
                Ok(Ok(v)) => v.to_string(),
                Ok(Err(m)) => format!("err {m}"),
                Err(_) => "panic".to_owned(),
            };
            failures.push(format!(
                "{op} {} ({})\n  want {} err {}\n  got  {}",
                Value::Array(a.clone()),
                rec["src"],
                rec["out"],
                rec["err"],
                got.chars().take(600).collect::<String>()
            ));
        }
    }
    let _ = std::panic::take_hook();
    assert!(
        n == header["records"].as_u64().unwrap() as usize,
        "record count"
    );
    if !failures.is_empty() {
        let shown: Vec<_> = failures.iter().take(25).cloned().collect();
        panic!(
            "{} of {n} records differ:\n{}",
            failures.len(),
            shown.join("\n")
        );
    }
}
