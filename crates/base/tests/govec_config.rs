//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/config.jsonl.gz`
//! (tracker `govec`, domain "config", block D00.2 part 2) against the Rust port of
//! `pkg/config` and of the BurntSushi TOML decoder.
//!
//! Records (tracker `govec/domains/config.go`): `load` (Config.Load of a TOML text, then
//! Config.Valid, with the config dumped field by field before and after Valid), JSON, clone,
//! merge, flatten, nullableBool / AtomicBool, the log / tracing / client-go conversions and
//! the global config helpers. Dumps use Go field names ("Config.Log.File.FileLogConfig.MaxSize=")
//! and Go's `%q` / `strconv` formatting; the default tmp-storage-path (temporary directory and user ID)
//! is replaced by a placeholder on both sides.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tidb_base::config::{self, Config, GoStruct, LoadError, NullableBool, ValRef, nbTrue, nbUnset};
use tidb_base::gofmt::{self, Arg};
use tidb_base::gostd::{self, json as gojson};
use tidb_base::{dbterror, errors, metrics};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

const CONF_NAME: &str = "govec-config.toml";

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/config.jsonl.gz",
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

fn q(s: &str) -> String {
    String::from_utf8(gofmt::sprintf("%q", &[Arg::from(s.to_owned())])).unwrap()
}

/// `os.TempDir()`.
fn os_temp_dir() -> String {
    match std::env::var("TMPDIR") {
        Ok(d) if !d.is_empty() => d,
        _ => "/tmp".to_owned(),
    }
}

/// `envClean` of the generator: the default tmp-storage-path as a placeholder.
fn env_clean(s: &str) -> String {
    s.replace(&config::temp_storage_dir_name(), "<TEMP_STORAGE_DIR>")
}

/// `tempClean` of the generator: `<TMPDIR>` and `<UID>_tidb` in a path built by
/// `encodeDefTempStorageDir`.
fn temp_clean(p: &str) -> String {
    let tmp = tidb_base::gostd::path::clean(&os_temp_dir());
    let mut p = p.to_owned();
    if p.starts_with(&format!("{tmp}/")) {
        p = format!("<TMPDIR>{}", &p[tmp.len()..]);
    }
    let uid = format!("{}_tidb", rustix::process::getuid().as_raw());
    p.split('/')
        .map(|part| if part == uid { "<UID>_tidb" } else { part })
        .collect::<Vec<_>>()
        .join("/")
}

/// `confDump` of the generator over the reflection interface.
fn conf_dump(s: &dyn GoStruct, path: &str, out: &mut Vec<String>) {
    for (i, f) in s.fields().iter().enumerate() {
        let p = format!("{path}.{}", f.go_name);
        let v = match s.field(i) {
            ValRef::Struct(sub) => {
                conf_dump(sub, &p, out);
                continue;
            }
            ValRef::NullableBool(b) => {
                out.push(format!("{p}.IsValid={}", b.is_valid));
                out.push(format!("{p}.IsTrue={}", b.is_true));
                continue;
            }
            ValRef::Bool(b) => b.to_string(),
            ValRef::Int(x) => x.to_string(),
            ValRef::I32(x) => x.to_string(),
            ValRef::I64(x) => x.to_string(),
            ValRef::Uint(x) => x.to_string(),
            ValRef::U32(x) => x.to_string(),
            ValRef::U64(x) => x.to_string(),
            ValRef::F64(x) => String::from_utf8(gofmt::sprintf("%v", &[Arg::from(*x)])).unwrap(),
            ValRef::Str(x) => q(x),
            ValRef::Duration(d) => d.0.to_string(),
            ValRef::StrSlice(None) | ValRef::StrMap(None) => "nil".to_owned(),
            ValRef::StrSlice(Some(v)) => {
                format!("[{}]", v.iter().map(|x| q(x)).collect::<Vec<_>>().join(" "))
            }
            ValRef::StrMap(Some(m)) => {
                format!(
                    "map[{}]",
                    m.iter()
                        .map(|(k, v)| format!("{}:{}", q(k), q(v)))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
            ValRef::AtomicBool(b) => b.load().to_string(),
        };
        out.push(format!("{p}={v}"));
    }
}

fn dump(s: &dyn GoStruct, root: &str) -> Value {
    let mut lines = Vec::new();
    conf_dump(s, root, &mut lines);
    Value::Array(lines.iter().map(|l| gs(env_clean(l).as_bytes())).collect())
}

fn msg(e: &[u8]) -> Value {
    gs(env_clean(&String::from_utf8_lossy(e)).as_bytes())
}

fn opt_msg<T>(r: &Result<T, errors::Error>) -> Value {
    match r {
        Ok(_) => Value::Null,
        Err(e) => msg(&e.error()),
    }
}

fn sections(s: &[config::InstanceConfigSection]) -> Value {
    Value::Array(
        s.iter()
            .map(|sec| {
                json!({"section": sec.section_name,
                    "mappings": sec.name_mappings.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()})
            })
            .collect(),
    )
}

fn reset_options() {
    config::ConflictOptions.lock().clear();
    config::DeprecatedOptions.lock().clear();
}

/// `loadConf`: a fresh config with `data` loaded.
fn load_conf(data: &[u8]) -> (Config, Result<(), LoadError>) {
    reset_options();
    let mut c = config::new_config();
    let r = c.load_data(CONF_NAME, data);
    (c, r)
}

fn is_decode_err(r: &Result<(), LoadError>) -> bool {
    matches!(r, Err(LoadError::Other(_)))
}

fn load_err(r: &Result<(), LoadError>) -> Value {
    match r {
        Ok(()) => Value::Null,
        Err(e) => msg(&e.error()),
    }
}

/// `loadConf` + the decode-error guard of the generator's ops.
fn load_ok(data: &[u8]) -> Result<Config, Value> {
    let (c, r) = load_conf(data);
    if is_decode_err(&r) {
        return Err(json!({"decode_err": load_err(&r)}));
    }
    Ok(c)
}

fn load_record(data: &[u8]) -> Value {
    let (mut c, r) = load_conf(data);
    let mut out = serde_json::Map::new();
    out.insert("load_err".into(), load_err(&r));
    let ty = match &r {
        Ok(()) => "",
        Err(LoadError::ValidationFailed(e)) => {
            out.insert("undecoded".into(), json!(e.undecoded_items));
            "validation"
        }
        Err(LoadError::InstanceSection(_)) => "instance",
        Err(LoadError::Other(_)) => {
            out.insert("load_err_type".into(), json!("other"));
            return Value::Object(out);
        }
    };
    out.insert("load_err_type".into(), json!(ty));
    out.insert("conflict".into(), sections(&config::ConflictOptions.lock()));
    out.insert(
        "deprecated".into(),
        sections(&config::DeprecatedOptions.lock()),
    );
    out.insert("conf".into(), dump(&c, "Config"));
    let v = c.valid();
    out.insert("valid_err".into(), opt_msg(&v));
    out.insert("conf_valid".into(), dump(&c, "Config"));
    Value::Object(out)
}

/// Writes `data` to a temporary file (for the ops that read a file); returns the path.
fn temp_file(data: &[u8]) -> String {
    let dir = std::env::temp_dir().join(format!("govec-config-rs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(CONF_NAME);
    std::fs::write(&p, data).unwrap();
    p.to_string_lossy().into_owned()
}

fn exec(op: &str, a: &[Value]) -> Value {
    let s = |i: usize| gs_bytes(&a[i]);
    let st = |i: usize| String::from_utf8(gs_bytes(&a[i])).unwrap();
    match op {
        "default_conf" => {
            json!({"new": dump(&config::new_config(), "Config"), "global": dump(&**config::get_global_config(), "Config")})
        }
        "defaults" => json!({
            "pessimistic": dump(&config::default_pessimistic_txn(), "PessimisticTxn"),
            "trx": dump(&config::default_trx_summary(), "TrxSummary"),
            "atomic_true": gs(&config::new_atomic_bool(true).marshal_text()),
        }),
        "load" => load_record(&s(0)),
        "load_missing" => {
            let mut c = config::new_config();
            let r = c.load(&st(0));
            json!({"err": load_err(&r)})
        }
        "removed_check" => {
            let p = temp_file(&s(0));
            let mut c = config::new_config();
            let r = c.removed_variable_check(&p);
            let m = match &r {
                Ok(()) => Value::Null,
                Err(e) => msg(String::from_utf8_lossy(&e.error())
                    .replace(&p, CONF_NAME)
                    .as_bytes()),
            };
            json!({"err": m})
        }
        "json_config" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let restore = config::restore_func();
            config::store_global_config(Arc::new(c));
            let r = config::get_json_config();
            restore();
            let j = r.as_ref().map(|s| env_clean(s)).unwrap_or_default();
            json!({"json": gs(j.as_bytes()), "err": opt_msg(&r)})
        }
        "contain_hidden" => json!(config::contain_hidden_config(&st(0))),
        "clone" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            match config::clone_conf(&c) {
                Ok(cl) => json!({"err": null, "conf": dump(&cl, "Config")}),
                Err(e) => json!({"err": msg(&e.error())}),
            }
        }
        "merge" => {
            let mut x = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let y = match load_ok(&s(1)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let (acc, rej) = config::merge_config_items(&mut x, &y);
            json!({"accepted": acc, "rejected": rej, "conf": dump(&x, "Config")})
        }
        "flatten" => {
            let m = match &mut gojson::unmarshal(&s(0)).unwrap() {
                gojson::Value::Object(m) => std::mem::take(m),
                v => panic!("not an object: {v:?}"),
            };
            let flat: BTreeMap<String, gojson::Value> = config::flatten_config_items(&m);
            gs(env_clean(&gojson::marshal(&gojson::Value::Object(flat))).as_bytes())
        }
        "nb_text" => {
            let mut b = nbUnset;
            let r = b.unmarshal_text(&s(0));
            json!({"err": opt_msg(&r), "valid": b.is_valid, "true": b.is_true,
                "text": gs(&b.marshal_text()), "json": gs(&b.marshal_json())})
        }
        "nb_json" => {
            let mut b: NullableBool = nbTrue;
            let r = b.unmarshal_json(&s(0));
            json!({"err": r.is_err(), "valid": b.is_valid, "true": b.is_true})
        }
        "ab_text" => {
            let mut b = config::new_atomic_bool(true);
            let r = b.unmarshal_text(&s(0));
            json!({"err": opt_msg(&r), "value": b.load(), "text": gs(&b.marshal_text())})
        }
        "log_config" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let lc = c.log.to_log_config();
            json!({
                "level": lc.level, "format": lc.format, "disable_timestamp": lc.disable_timestamp,
                "file": dump(&lc.file, "File"), "disable_error_verbose": lc.disable_error_verbose,
                "timeout": lc.timeout, "slow_query_file": lc.slow_query_file, "general_log_file": lc.general_log_file,
            })
        }
        "tracing_config" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let t = c.open_tracing.to_tracing_config();
            json!({
                "disabled": t.disabled, "rpc_metrics": t.rpc_metrics,
                "reporter": [t.reporter.queue_size, t.reporter.buffer_flush_interval.0, t.reporter.log_spans,
                    t.reporter.local_agent_host_port],
                "sampler": [t.sampler.type_, String::from_utf8(gofmt::sprintf("%v", &[Arg::from(t.sampler.param)])).unwrap(),
                    t.sampler.sampling_server_url, t.sampler.max_operations, t.sampler.sampling_refresh_interval.0],
            })
        }
        "tikv_config" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            dump(&c.get_tikv_config(), "TikvConfig")
        }
        "global" => {
            let c = match load_ok(&s(0)) {
                Ok(c) => c,
                Err(v) => return v,
            };
            let restore = config::restore_func();
            config::update_global(|conf| *conf = c);
            let out = json!({
                "table_lock": config::table_lock_enabled(), "delay_clean": config::table_lock_delay_clean(),
                "keyspace": config::get_global_keyspace_name(), "txn_scope": config::get_txn_scope_from_config(),
            });
            restore();
            out
        }
        "temp_storage" => {
            let mut c = config::new_config();
            c.host = a[0].as_str().unwrap().to_owned();
            c.port = a[1].as_u64().unwrap() as usize;
            c.status.status_host = a[2].as_str().unwrap().to_owned();
            c.status.status_port = a[3].as_u64().unwrap() as usize;
            let p = a[4].as_str().unwrap();
            if !p.is_empty() {
                c.temp_storage_path = p.to_owned();
            }
            c.update_temp_storage_path();
            gs(temp_clean(&c.temp_storage_path).as_bytes())
        }
        "error_label" => {
            let err = match st(0).as_str() {
                "plain" => errors::new(st(1)),
                _ => dbterror::ClassExecutor.new_std(st(1).parse::<u16>().unwrap()),
            };
            json!(metrics::execute_error_to_label(&err))
        }
        "f2i" => {
            use tidb_base::gostd::conv;
            let f = f64::from_bits(st(0).parse::<u64>().unwrap());
            let v = match st(1).as_str() {
                "int64" | "int" => conv::f64_to_i64(f).to_string(),
                "int32" => conv::f64_to_i32(f).to_string(),
                "int16" => conv::f64_to_i16(f).to_string(),
                "int8" => conv::f64_to_i8(f).to_string(),
                "uint64" | "uint" => conv::f64_to_u64(f).to_string(),
                "uint32" => conv::f64_to_u32(f).to_string(),
                "uint16" => conv::f64_to_u16(f).to_string(),
                "uint8" => conv::f64_to_u8(f).to_string(),
                t => panic!("type {t}"),
            };
            json!(v)
        }
        "simple_fold_table" => Value::Array(
            (0..=0x10FFFFu32)
                .filter_map(char::from_u32)
                .filter_map(|r| {
                    let f = gostd::simple_fold(r);
                    (f != r).then(|| json!([r as u32, f as u32]))
                })
                .collect(),
        ),
        "case_table" => Value::Array(
            (0..=0x10FFFFu32)
                .filter_map(char::from_u32)
                .filter_map(|r| {
                    let (u, l) = (gostd::to_upper_rune(r), gostd::to_lower_rune(r));
                    (u != r || l != r).then(|| json!([r as u32, u as u32, l as u32]))
                })
                .collect(),
        ),
        "equal_fold" => json!(gostd::equal_fold(&st(0), &st(1))),
        _ => panic!("unknown op {op}"),
    }
}

#[test]
fn govec_config() {
    let mut gz = flate2::read::GzDecoder::new(
        std::fs::File::open(vectors()).unwrap_or_else(|e| panic!("{}: {e}", vectors())),
    );
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "config");
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
            (Ok(v), None) => *v == rec["out"],
            _ => false,
        };
        if !ok {
            if let Ok(path) = std::env::var("GOVEC_FAILURES") {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .unwrap();
                let g = match &got {
                    Ok(v) => v.clone(),
                    Err(_) => json!("panic"),
                };
                writeln!(
                    f,
                    "{}",
                    json!({"op": op, "args": a, "want": rec["out"], "got": g})
                )
                .unwrap();
            }
            let got = match &got {
                Ok(v) => v.to_string(),
                Err(_) => "panic".to_owned(),
            };
            failures.push(format!(
                "{op} {} ({})\n  want {} err {}\n  got  {}",
                Value::Array(a.clone()),
                rec["src"],
                rec["out"]
                    .to_string()
                    .chars()
                    .take(3000)
                    .collect::<String>(),
                rec["err"],
                got.chars().take(3000).collect::<String>()
            ));
        }
    }
    let _ = std::panic::take_hook();
    assert_eq!(header["records"].as_u64().unwrap() as usize, n);
    if !failures.is_empty() {
        let shown: Vec<_> = failures.iter().take(15).cloned().collect();
        panic!(
            "{} of {n} records differ:\n{}",
            failures.len(),
            shown.join("\n")
        );
    }
}
