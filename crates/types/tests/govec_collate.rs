//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/collate.jsonl.gz`
//! (tracker `govec`, domain `collate`: Go TiDB v8.5.8 `pkg/util/collate`,
//! `pkg/parser/charset/charset.go` and `pkg/util/stringutil`) against the Rust port.
//!
//! One `#[test]`: the new-collation switch is global state, and most records start by
//! setting it (`args[0]`), so they replay in file order on one thread. Strings are
//! `vec.GoString` (invalid UTF-8 as `{"hex": ...}`). `out` must be equal as JSON, errors
//! must match by message, and `err.name == "panic"` records must panic in Rust too.
//! `key_digest` records hash `Key`, `KeyWithoutTrimRightSpace` and the `Compare` with the
//! previous code point over a range of code points, as the Go generator does.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::mysql::{ModeANSIQuotes, SQLMode};
use tidb_parser::gocharset::{self as charset, Charset, Collation};
use tidb_types::collate::{self, Collator};
use tidb_types::stringutil;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/collate.jsonl.gz",
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

/// Go's `%T` of a collator.
fn type_name(c: &dyn Collator) -> &'static str {
    let a = c.as_any();
    macro_rules! name {
        ($($t:ident => $n:literal),* $(,)?) => {
            $(if a.is::<collate::$t>() {
                return concat!("*collate.", $n);
            })*
        };
    }
    name!(
        BinCollator => "binCollator",
        BinPaddingCollator => "binPaddingCollator",
        DerivedBinCollator => "derivedBinCollator",
        GbkBinCollator => "gbkBinCollator",
        GbkChineseCICollator => "gbkChineseCICollator",
        GeneralCICollator => "generalCICollator",
        UnicodeCICollator => "unicodeCICollator",
        Unicode0900AICICollator => "unicode0900AICICollator",
        ZhPinyinTiDBASCSCollator => "zhPinyinTiDBASCSCollator",
    );
    panic!("unknown collator type")
}

fn collation_out(c: Option<&Arc<Collation>>) -> Value {
    match c {
        None => Value::Null,
        Some(c) => json!({
            "id": c.id, "charset": c.charset_name, "name": c.name, "default": c.is_default(),
            "sortlen": c.sortlen, "pad": c.pad_attribute,
        }),
    }
}

fn charset_out(c: Option<&Arc<Charset>>) -> Value {
    match c {
        None => Value::Null,
        Some(c) => {
            let mut names: Vec<String> = c
                .collations()
                .iter()
                .map(|(n, co)| {
                    if co.is_default() {
                        format!("{n}*")
                    } else {
                        n.clone()
                    }
                })
                .collect();
            names.sort();
            json!({
                "name": c.name, "default": c.default_collation(), "desc": c.desc,
                "maxlen": c.maxlen, "collations": names,
            })
        }
    }
}

/// Go `string(rune(r))`: surrogates and out-of-range values become U+FFFD.
fn go_str(r: i64) -> Vec<u8> {
    let c = u32::try_from(r)
        .ok()
        .and_then(char::from_u32)
        .unwrap_or('\u{FFFD}');
    c.to_string().into_bytes()
}

fn key_digest(name: &str, lo: i64, hi: i64) -> String {
    let c = collate::get_collator(name);
    let mut h = Sha256::new();
    let mut put = |r: std::thread::Result<Vec<u8>>| match r {
        Err(_) => h.update([0xFF]),
        Ok(out) => {
            h.update((out.len() as u16).to_le_bytes());
            h.update(&out);
        }
    };
    for r in lo..hi {
        let s = go_str(r);
        put(catch_unwind(AssertUnwindSafe(|| c.key(&s))));
        put(catch_unwind(AssertUnwindSafe(|| {
            c.key_without_trim_right_space(&s)
        })));
        let p = go_str(r - 1);
        put(catch_unwind(AssertUnwindSafe(|| {
            vec![c.compare(&s, &p) as i8 as u8]
        })));
    }
    hex(&h.finalize())
}

fn err_str(e: &Error) -> Value {
    gs(&e.error())
}

fn exec(op: &str, args: &[Value]) -> (Value, Option<Error>) {
    let s = |i: usize| ungs(&args[i]);
    let st = |i: usize| args[i].as_str().unwrap().to_owned();
    let int = |i: usize| args[i].as_i64().unwrap();
    let set_enabled = || collate::set_new_collation_enabled_for_test(int(0) == 1);
    match op {
        "compare" => {
            set_enabled();
            let c = collate::get_collator(&st(1));
            (json!(c.compare(&s(2), &s(3))), None)
        }
        "key" => {
            set_enabled();
            let c = collate::get_collator(&st(1));
            let x = s(2);
            (
                json!({ "key": hex(&c.key(&x)), "key_wo": hex(&c.key_without_trim_right_space(&x)) }),
                None,
            )
        }
        "key_digest" => {
            collate::set_new_collation_enabled_for_test(true);
            (json!(key_digest(&st(0), int(1), int(2))), None)
        }
        "pattern" => {
            set_enabled();
            let esc = int(3) as u8;
            let mut p = collate::get_collator(&st(1)).pattern();
            p.compile(&s(2), esc);
            let res: Vec<bool> = args[4]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| p.do_match(&ungs(x)))
                .collect();
            let (w, t) = stringutil::compile_pattern(&s(2), esc);
            let (wb, tb) = stringutil::compile_pattern_binary(&s(2), esc);
            let ws: Vec<u32> = w.iter().map(|&r| r as u32).collect();
            (
                json!({ "match": res, "weights": ws, "types": hex(&t),
                        "bin_weights": hex(&wb), "bin_types": hex(&tb) }),
                None,
            )
        }
        "collator" => {
            set_enabled();
            let c: &dyn Collator = match st(1).as_str() {
                "name" => collate::get_collator(&st(2)),
                "id" => collate::get_collator_by_id(int(2) as isize),
                "bin" => collate::convert_and_get_bin_collator(&st(2)),
                "binary" => collate::get_binary_collator(),
                "slice" => {
                    let n = int(2);
                    assert!(n >= 0, "makeslice: len out of range");
                    let sl = collate::get_binary_collator_slice(n as usize);
                    let ts: Vec<&str> = sl.iter().map(|x| type_name(*x)).collect();
                    let ts = if ts.is_empty() {
                        Value::Null
                    } else {
                        json!(ts)
                    };
                    return (json!({ "len": sl.len(), "types": ts }), None);
                }
                k => panic!("collator kind {k}"),
            };
            let cl = c.clone_collator();
            (
                json!({
                    "type": type_name(c), "raw": collate::can_use_raw_mem_as_key(c),
                    "enabled": collate::new_collation_enabled(),
                    "clone": type_name(&*cl), "clone_raw": collate::can_use_raw_mem_as_key(&*cl),
                }),
                None,
            )
        }
        "collation_id" => {
            set_enabled();
            let id = int(1) as i32;
            (
                json!({
                    "rewrite": collate::rewrite_new_collation_id_if_needed(id),
                    "restore": collate::restore_collation_id_if_needed(id),
                    "id2name": collate::collation_id2_name(id),
                    "proto2coll": collate::proto_to_collation(id),
                }),
                None,
            )
        }
        "collation_name" => {
            set_enabled();
            let n = st(1);
            let mut o = json!({
                "name2id": collate::collation_name2_id(&n),
                "to_proto": collate::collation_to_proto(&n),
                "default_utf8mb4": collate::is_default_collation_for_utf8mb4(&n),
                "ci": collate::is_ci_collation(&n),
                "to_bin": collate::convert_and_get_bin_collation(&n),
                "bin": collate::is_bin_collation(&n),
                "pad_space": collate::is_pad_space_collation(&n),
            });
            let m = o.as_object_mut().unwrap();
            match collate::get_collation_by_name(&n) {
                Ok(c) => {
                    m.insert("by_name".into(), collation_out(Some(&c)));
                }
                Err(e) => {
                    m.insert("by_name".into(), Value::Null);
                    m.insert("by_name_err".into(), err_str(&e));
                }
            }
            match charset::get_collation_by_name(&n) {
                Ok(c) => {
                    m.insert("charset_by_name".into(), collation_out(Some(&c)));
                }
                Err(e) => {
                    m.insert("charset_by_name".into(), Value::Null);
                    m.insert("charset_by_name_err".into(), err_str(&e));
                }
            }
            let sub = catch_unwind(AssertUnwindSafe(|| {
                collate::substitute_missing_collation_to_default(&n)
            }));
            m.insert(
                "substitute".into(),
                match sub {
                    Ok(x) => json!(x),
                    Err(_) => json!("panic"),
                },
            );
            (o, None)
        }
        "compatible" => (json!(collate::compatible_collate(&st(0), &st(1))), None),
        "supported" => {
            set_enabled();
            let ns: Vec<Value> = collate::get_supported_collations()
                .iter()
                .map(|c| collation_out(Some(c)))
                .collect();
            let ps: Vec<Value> = charset::get_supported_collations()
                .iter()
                .map(|c| collation_out(Some(c)))
                .collect();
            (json!({ "collate": ns, "charset": ps }), None)
        }
        "charset" => {
            set_enabled();
            let n = st(1);
            let mut o = serde_json::Map::new();
            let (info, err) = charset::get_charset_info(&n);
            o.insert("info".into(), charset_out(info.as_ref()));
            if let Err(e) = err {
                o.insert("info_err".into(), err_str(&e));
            }
            match charset::get_default_collation(&n) {
                Ok(d) => {
                    o.insert("default".into(), json!(d));
                }
                Err(e) => {
                    o.insert("default".into(), json!(""));
                    o.insert("default_err".into(), err_str(&e));
                }
            }
            match charset::get_default_collation_legacy(&n) {
                Ok(d) => {
                    o.insert("legacy".into(), json!(d));
                }
                Err(e) => {
                    o.insert("legacy".into(), json!(""));
                    o.insert("legacy_err".into(), err_str(&e));
                }
            }
            let valid: Vec<bool> = args[2]
                .as_array()
                .unwrap()
                .iter()
                .map(|co| charset::valid_charset_and_collation(&n, co.as_str().unwrap()))
                .collect();
            let valid = if valid.is_empty() {
                Value::Null
            } else {
                json!(valid)
            };
            o.insert("valid".into(), valid);
            (Value::Object(o), None)
        }
        "charset_by_id" => {
            set_enabled();
            let id = int(1) as isize;
            let (cs, co, err) = charset::get_charset_info_by_id(id);
            let mut o = json!({ "charset": cs, "collation": co });
            let m = o.as_object_mut().unwrap();
            match charset::get_collation_by_id(id) {
                Ok(c) => {
                    m.insert("by_id".into(), collation_out(Some(&c)));
                }
                Err(e) => {
                    m.insert("by_id".into(), Value::Null);
                    m.insert("by_id_err".into(), err_str(&e));
                }
            }
            (o, err.err())
        }
        "charsets" => {
            set_enabled();
            let css: Vec<Value> = charset::get_supported_charsets()
                .iter()
                .map(|c| charset_out(Some(c)))
                .collect();
            let (dc, dco) = charset::get_default_charset_and_collate();
            (json!({ "charsets": css, "default": [dc, dco] }), None)
        }
        "gbk_encode" => match charset::encoding_gbk::new_custom_gbk_encoder().bytes(&s(0)) {
            Ok(b) => (json!(hex(&b)), None),
            Err(e) => (json!(""), Some(e)),
        },
        "unquote" => match stringutil::unquote(&s(0)) {
            Ok(t) => (gs(&t), None),
            Err(e) => (json!(""), Some(e)),
        },
        "unquote_char" => {
            let x = s(0);
            match stringutil::unquote_char(&x, int(1) as u8) {
                Ok((v, tail)) => (json!({ "value": hex(&v), "tail": gs(tail) }), None),
                Err(e) => (json!({ "value": "", "tail": "" }), Some(e)),
            }
        }
        "like2regexp" => (gs(stringutil::compile_like2_regexp(&s(0)).as_bytes()), None),
        "is_exact_match" => {
            let (_, t) = stringutil::compile_pattern(&s(0), int(1) as u8);
            (json!(stringutil::is_exact_match(&t)), None)
        }
        "string_misc" => {
            let x = s(0);
            let mut low = x.clone();
            stringutil::lower_one_string(&mut low);
            // StringerStr holds a Rust `String` (it is for `Display` in logs), so it cannot
            // carry invalid UTF-8 as Go's `StringerStr` (a Go string) does: those records
            // are not compared on this field (spec §3).
            let stringer = match String::from_utf8(x.clone()) {
                Ok(t) => gs(stringutil::StringerStr(t).to_string().as_bytes()),
                Err(_) => json!(STRINGER_NOT_UTF8),
            };
            (
                json!({
                    "copy": gs(&stringutil::copy(&x)),
                    "escape": gs(&stringutil::escape(&x, SQLMode(0))),
                    "escape_ansi": gs(&stringutil::escape(&x, ModeANSIQuotes)),
                    "tail_spaces": stringutil::get_tail_space_count(&x),
                    "glob": gs(&stringutil::escape_glob_question_mark(&x)),
                    "lower": gs(&low),
                    "stringer": stringer,
                }),
                None,
            )
        }
        "lower_exclude" => {
            let mut b = s(0);
            let e = stringutil::lower_one_string_exclude_escape_char(&mut b, int(1) as u8);
            (json!({ "str": gs(&b), "escape": e }), None)
        }
        "byte_props" => {
            let c = int(0) as u8;
            (
                json!([
                    stringutil::utf8_len(c),
                    stringutil::is_upper_ascii(c),
                    stringutil::is_lower_ascii(c),
                    stringutil::is_numeric_ascii(c)
                ]),
                None,
            )
        }
        "trim_utf8" => {
            let all = s(0);
            let mut x: &[u8] = &all;
            let n = stringutil::trim_utf8_string(&mut x, int(1));
            let pos = stringutil::convert_pos_in_utf8(&all, int(2));
            (json!({ "trimmed": n, "rest": gs(x), "pos": pos }), None)
        }
        "labels" => {
            let m: HashMap<String, String> = args[0]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                .collect();
            (json!(stringutil::build_string_from_labels(&m)), None)
        }
        _ => panic!("unknown op {op}"),
    }
}

/// Marks a `string_misc` record whose `stringer` field is not compared (invalid UTF-8).
const STRINGER_NOT_UTF8: &str = "<not compared: invalid UTF-8>";

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
    let (mut out, err) = match got {
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
    if op == "string_misc" && out["stringer"] == STRINGER_NOT_UTF8 {
        out["stringer"] = rec["out"]["stringer"].clone();
    }
    if out != rec["out"] {
        return Some(format!("out: got {out}, want {}", rec["out"]));
    }
    let got_err = err.as_ref().map(err_str);
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
fn govec_collate() {
    // Go's initial state: new collations enabled (TiDB's default), gbk's default collation
    // gbk_bin (charset's init does not call switchDefaultCollation).
    assert!(collate::new_collation_enabled());
    assert_eq!(charset::get_default_collation("gbk").unwrap(), "gbk_bin");

    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "collate");
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
