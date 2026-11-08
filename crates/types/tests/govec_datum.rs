//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/datum.jsonl.gz`
//! (tracker `govec`, domain `datum`: block D01.1 part 1 with D01.4 — convert.go,
//! overflow.go, truncate.go, binary_literal.go, enum.go, set.go, compare.go, helper.go,
//! etc.go — and the charset Encodings of pkg/parser/charset) against the Rust port.
//!
//! Every record's `out` must be equal as JSON; errors and warnings must match by code and
//! message; `err.name == "panic"` records must panic in Rust too. Floats are IEEE bits.

use serde_json::{Value, json};
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use tidb_base::Error;
use tidb_base::contextutil::WarnAppender;
use tidb_base::gostd::time::utc;
use tidb_parser::gocharset::encoding as enc;
use tidb_parser::types::FieldType;
use tidb_types::binary_literal::{self as bl, BinaryLiteral};
use tidb_types::context::{Context, Flags, new_context};
use tidb_types::json::BinaryJSON;
use tidb_types::time::{Duration, Time};
use tidb_types::{
    MyDecimal, collate, compare, convert, enum_type, etc, helper, overflow, set_type,
};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn vectors() -> String {
    format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/datum.jsonl.gz",
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

fn err_json(e: &Error) -> Value {
    json!({ "code": e.code(), "msg": gs(&e.error()) })
}

fn named_err(name: &str, e: &Error) -> Value {
    json!({ "name": name, "code": e.code(), "msg": gs(&e.error()) })
}

#[derive(Default)]
struct Warns(Mutex<Vec<Error>>);

impl WarnAppender for Warns {
    fn append_warning(&self, err: Error) {
        self.0.lock().unwrap().push(err);
    }
    fn append_note(&self, msg: Error) {
        self.0.lock().unwrap().push(msg);
    }
}

type Got = (Value, Option<Error>, Vec<Error>);

fn ctx(a: &[Value], i: usize) -> (Context, Arc<Warns>) {
    let w = Arc::new(Warns::default());
    (
        new_context(Flags(a[i].as_u64().unwrap() as u16), utc(), w.clone()),
        w,
    )
}

fn time_out(t: &Time) -> Value {
    json!({ "raw": t.raw().0, "s": gs(&t.string()) })
}

fn dur_out(d: &Duration) -> Value {
    json!({ "d": d.duration, "fsp": d.fsp, "s": gs(&d.string()) })
}

fn dec_out(d: &MyDecimal) -> Value {
    let j: Value = serde_json::from_slice(&d.marshal_json().unwrap()).unwrap();
    let mut st = vec![
        j["DigitsInt"].clone(),
        j["DigitsFrac"].clone(),
        j["ResultFrac"].clone(),
        json!(i64::from(j["Negative"].as_bool().unwrap())),
    ];
    st.extend(j["WordBuf"].as_array().unwrap().iter().cloned());
    json!({ "s": String::from_utf8(d.to_string()).unwrap(), "str": d.string(), "st": st })
}

fn f(v: &Value) -> f64 {
    f64::from_bits(v.as_u64().unwrap())
}

fn i(v: &Value) -> i64 {
    v.as_i64().unwrap()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap()
}

fn s(v: &Value) -> Vec<u8> {
    ungs(v)
}

fn r<T: Into<Value>>(res: Result<T, Error>) -> (Value, Option<Error>) {
    match res {
        Ok(v) => (v.into(), None),
        Err(e) => (json!(0), Some(e)),
    }
}

fn rv<T: Into<Value>>(v: T, res: Result<(), Error>) -> (Value, Option<Error>) {
    (v.into(), res.err())
}

fn elems(v: &Value) -> Vec<Vec<u8>> {
    v.as_array()
        .map(|a| a.iter().map(ungs).collect())
        .unwrap_or_default()
}

fn bj(v: &Value) -> BinaryJSON<'static> {
    BinaryJSON::owned(v[0].as_u64().unwrap() as u8, unhex(v[1].as_str().unwrap()))
}

fn exec(op: &str, a: &[Value]) -> Got {
    collate::set_new_collation_enabled_for_test(true);
    if let Some(g) = part2::exec(op, a) {
        return g;
    }
    let nw = |(v, e): (Value, Option<Error>)| (v, e, vec![]);
    let ww = |(v, e): (Value, Option<Error>), w: Arc<Warns>| (v, e, w.0.lock().unwrap().clone());
    match op {
        "overflow" => nw(match a[0].as_str().unwrap() {
            "AddUint64" => r(overflow::add_uint64(u(&a[1]), u(&a[2]))),
            "AddInt64" => r(overflow::add_int64(i(&a[1]), i(&a[2]))),
            "AddDuration" => r(overflow::add_duration(i(&a[1]), i(&a[2]))),
            "SubDuration" => r(overflow::sub_duration(i(&a[1]), i(&a[2]))),
            "AddInteger" => r(overflow::add_integer(u(&a[1]), i(&a[2]))),
            "SubUint64" => r(overflow::sub_uint64(u(&a[1]), u(&a[2]))),
            "SubInt64" => r(overflow::sub_int64(i(&a[1]), i(&a[2]))),
            "SubUintWithInt" => r(overflow::sub_uint_with_int(u(&a[1]), i(&a[2]))),
            "SubIntWithUint" => r(overflow::sub_int_with_uint(i(&a[1]), u(&a[2]))),
            "MulUint64" => r(overflow::mul_uint64(u(&a[1]), u(&a[2]))),
            "MulInt64" => r(overflow::mul_int64(i(&a[1]), i(&a[2]))),
            "MulInteger" => r(overflow::mul_integer(u(&a[1]), i(&a[2]))),
            "DivInt64" => r(overflow::div_int64(i(&a[1]), i(&a[2]))),
            "DivUintWithInt" => r(overflow::div_uint_with_int(u(&a[1]), i(&a[2]))),
            "DivIntWithUint" => r(overflow::div_int_with_uint(i(&a[1]), u(&a[2]))),
            fname => panic!("unknown {fname}"),
        }),
        "float_helper" => {
            let (x, p, q) = (f(&a[0]), i(&a[1]) as isize, i(&a[2]) as isize);
            let (tf, terr) = helper::truncate_float(x, p, q);
            let te = match terr {
                Ok(()) => Value::Null,
                Err(e) => named_err("ErrOverflow", &e),
            };
            nw((
                json!({
                    "round_float": helper::round_float(x).to_bits(),
                    "round": helper::round(x, p).to_bits(),
                    "truncate": helper::truncate(x, p).to_bits(),
                    "max_float": helper::get_max_float(p, q).to_bits(),
                    "tf": tf.to_bits(), "tf_err": te,
                    "tf_str": helper::truncate_float_to_string(x, p),
                }),
                None,
            ))
        }
        "length_precision" => {
            let (l, sc, un) = (
                i(&a[0]) as isize,
                i(&a[1]) as isize,
                a[2].as_bool().unwrap(),
            );
            nw((
                json!([
                    helper::decimal_length2_precision(l, sc, un),
                    helper::precision2_length_no_truncation(l, sc, un)
                ]),
                None,
            ))
        }
        "helper_str_to_int" => {
            let (v, e) = helper::str_to_int(&s(&a[0]));
            nw(rv(v, e))
        }
        "int_bounds" => {
            let tp = u(&a[0]) as u8;
            nw((
                json!({ "uu": convert::integer_unsigned_upper_bound(tp), "su": convert::integer_signed_upper_bound(tp),
                    "sl": convert::integer_signed_lower_bound(tp) }),
                None,
            ))
        }
        "convert_num" => {
            let flags = Flags(u(&a[1]) as u16);
            let tp = u(&a[5]) as u8;
            nw(match a[0].as_str().unwrap() {
                "FloatToInt" => {
                    let (v, e) = convert::convert_float_to_int(f(&a[2]), i(&a[3]), i(&a[4]), tp);
                    rv(v, e)
                }
                "IntToInt" => {
                    let (v, e) = convert::convert_int_to_int(i(&a[2]), i(&a[3]), i(&a[4]), tp);
                    rv(v, e)
                }
                "UintToInt" => {
                    let (v, e) = convert::convert_uint_to_int(u(&a[2]), i(&a[4]), tp);
                    rv(v, e)
                }
                "IntToUint" => {
                    let (v, e) = convert::convert_int_to_uint(flags, i(&a[2]), u(&a[4]), tp);
                    rv(v, e)
                }
                "UintToUint" => {
                    let (v, e) = convert::convert_uint_to_uint(u(&a[2]), u(&a[4]), tp);
                    rv(v, e)
                }
                "FloatToUint" => {
                    let (v, e) = convert::convert_float_to_uint(flags, f(&a[2]), u(&a[4]), tp);
                    rv(v, e)
                }
                fname => panic!("unknown {fname}"),
            })
        }
        "convert_decimal_to_uint" => {
            let mut d = MyDecimal::default();
            let _ = d.from_string(&s(&a[0]));
            let (v, e) = convert::convert_decimal_to_uint(&d, u(&a[1]), u(&a[2]) as u8);
            nw(rv(v, e))
        }
        "str_to_num" => {
            let (c, w) = ctx(a, 0);
            let (st, fc) = (s(&a[2]), a[3].as_bool().unwrap());
            let got = match a[1].as_str().unwrap() {
                "int" => {
                    let (v, e) = convert::str_to_int(&c, &st, fc);
                    rv(v, e)
                }
                "uint" => {
                    let (v, e) = convert::str_to_uint(&c, &st, fc);
                    rv(v, e)
                }
                _ => {
                    let (v, e) = convert::str_to_float(&c, &st, fc);
                    rv(v.to_bits(), e)
                }
            };
            ww(got, w)
        }
        "str_to_time" => {
            let (c, w) = ctx(a, 0);
            let (st, fsp) = (s(&a[2]), i(&a[3]) as isize);
            let got = if a[1] == "datetime" {
                let (t, e) = convert::str_to_date_time(&c, &st, fsp);
                rv(time_out(&t), e)
            } else {
                let (d, t, is_dur, e) = convert::str_to_duration(&c, &st, fsp);
                rv(
                    json!({ "d": dur_out(&d), "t": time_out(&t), "is_dur": is_dur }),
                    e,
                )
            };
            ww(got, w)
        }
        "number_to_duration" => {
            let (d, e) = convert::number_to_duration(i(&a[0]), i(&a[1]) as isize);
            nw(rv(dur_out(&d), e))
        }
        "convert_json" => {
            let (c, w) = ctx(a, 0);
            let j = bj(&a[2]);
            let got = match a[1].as_str().unwrap() {
                "int64" => {
                    let (v, e) = convert::convert_json_to_int64(&c, &j, a[3].as_bool().unwrap());
                    rv(v, e)
                }
                "int" => {
                    let (v, e) = convert::convert_json_to_int(
                        &c,
                        &j,
                        a[3].as_bool().unwrap(),
                        u(&a[4]) as u8,
                    );
                    rv(v, e)
                }
                "float" => {
                    let (v, e) = convert::convert_json_to_float(&c, &j);
                    rv(v.to_bits(), e)
                }
                _ => {
                    let (v, e) = convert::convert_json_to_decimal(&c, &j);
                    rv(dec_out(&v), e)
                }
            };
            ww(got, w)
        }
        "truncate_str" => nw((
            gs(convert::truncate_str(&s(&a[0]), i(&a[1]) as isize)),
            None,
        )),
        "handle_truncate" => {
            let (c, w) = ctx(a, 0);
            use tidb_base::gofmt::Arg;
            use tidb_types::errors::*;
            let e: Result<(), Error> =
                match a[1].as_str().unwrap() {
                    "nil" => Ok(()),
                    "plain" => Err(tidb_base::errors::new("plain error")),
                    "ErrTruncatedWrongVal" => Err(ErrTruncatedWrongVal
                        .gen_with_stack_by_args(vec![Arg::from("DOUBLE"), Arg::from("x")])),
                    "ErrTruncated" => Err(ErrTruncated.share()),
                    "ErrOverflow" => Err(ErrOverflow
                        .gen_with_stack_by_args(vec![Arg::from("BIGINT"), Arg::from("1")])),
                    "ErrBadNumber" => Err(ErrBadNumber.share()),
                    "ErrDataTooLong" => {
                        Err(ErrDataTooLong.gen_with_stack_by_args(vec![Arg::from("c")]))
                    }
                    "ErrWarnDataOutOfRange" => Err(ErrWarnDataOutOfRange
                        .gen_with_stack_by_args(vec![Arg::from("c"), Arg::Int(1, "int")])),
                    "ErrIncorrectDatetimeValue" => {
                        Err(ErrIncorrectDatetimeValue.gen_with_stack_by_args(vec![Arg::from("x")]))
                    }
                    "ErrDatetimeFunctionOverflow" => Err(ErrDatetimeFunctionOverflow
                        .gen_with_stack_by_args(vec![Arg::from("datetime")])),
                    "ErrWrongValue" => {
                        Err(ErrWrongValue
                            .gen_with_stack_by_args(vec![Arg::from("x"), Arg::from("y")]))
                    }
                    "ErrInvalidCharacterString" => Err(
                        tidb_parser::gocharset::encoding_gbk::ErrInvalidCharacterString
                            .gen_with_stack_by_args(vec![Arg::from("gbk"), Arg::from("x")]),
                    ),
                    n => panic!("unknown {n}"),
                };
            let got = (Value::Null, c.handle_truncate(e).err());
            ww(got, w)
        }
        "binlit_from_uint" => nw((
            json!(hex(&bl::new_binary_literal_from_uint(
                u(&a[0]),
                i(&a[1]) as isize
            )
            .0)),
            None,
        )),
        "binlit" => {
            let (c, w) = ctx(a, 0);
            let b = BinaryLiteral(unhex(a[1].as_str().unwrap()));
            let b2 = BinaryLiteral(unhex(a[2].as_str().unwrap()));
            let (v, ierr) = b.to_int(&c);
            let ie = match ierr {
                Ok(()) => Value::Null,
                Err(e) => named_err("ErrTruncatedWrongVal", &e),
            };
            let got = json!({
                "s": b.string(), "ts": gs(&b.to_string()),
                "bit": b.to_bit_literal_string(false), "bit_trim": b.to_bit_literal_string(true),
                "int": v, "int_err": ie, "cmp": b.compare(&b2),
            });
            ww((got, None), w)
        }
        "parse_bit_hex" => {
            let st = s(&a[1]);
            if a[0] == "bit" {
                let res = bl::parse_bit_str(&st);
                let nb = bl::new_bit_literal(&st);
                assert_eq!(
                    res.is_ok(),
                    nb.is_ok(),
                    "ParseBitStr and NewBitLiteral disagree"
                );
                let ts = nb.map(|x| x.to_string()).unwrap_or_default();
                return match res {
                    Ok(b) => (
                        json!({ "b": hex(&b.0), "nil": false, "ts": gs(&ts) }),
                        None,
                        vec![],
                    ),
                    Err(e) => (
                        json!({ "b": "", "nil": true, "ts": gs(&ts) }),
                        Some(e),
                        vec![],
                    ),
                };
            }
            let res = bl::parse_hex_str(&st);
            let nh = bl::new_hex_literal(&st);
            assert_eq!(
                res.is_ok(),
                nh.is_ok(),
                "ParseHexStr and NewHexLiteral disagree"
            );
            let ts = nh.map(|x| x.to_string()).unwrap_or_default();
            match res {
                Ok(b) => (
                    json!({ "b": hex(&b.0), "nil": false, "ts": gs(&ts) }),
                    None,
                    vec![],
                ),
                Err(e) => (
                    json!({ "b": "", "nil": true, "ts": gs(&ts) }),
                    Some(e),
                    vec![],
                ),
            }
        }
        "enum" => {
            let el = elems(&a[1]);
            let res = match a[0].as_str().unwrap() {
                "parse" => enum_type::parse_enum(&el, &s(&a[2]), a[3].as_str().unwrap()),
                "name" => enum_type::parse_enum_name(&el, &s(&a[2]), a[3].as_str().unwrap()),
                _ => enum_type::parse_enum_value(&el, u(&a[2])),
            };
            let (e, err) = match res {
                Ok(e) => (e, None),
                Err(err) => (enum_type::Enum::default(), Some(err)),
            };
            let c = e.copy();
            (
                json!({ "name": gs(&e.name), "value": e.value, "s": gs(&c.string()), "num": e.to_number().to_bits() }),
                err,
                vec![],
            )
        }
        "set" => {
            let el = elems(&a[1]);
            let res = match a[0].as_str().unwrap() {
                "parse" => set_type::parse_set(&el, &s(&a[2]), a[3].as_str().unwrap()),
                "name" => set_type::parse_set_name(&el, &s(&a[2]), a[3].as_str().unwrap()),
                _ => set_type::parse_set_value(&el, u(&a[2])),
            };
            let (e, err) = match res {
                Ok(e) => (e, None),
                Err(err) => (set_type::Set::default(), Some(err)),
            };
            let c = e.copy();
            (
                json!({ "name": gs(&e.name), "value": e.value, "s": gs(&c.string()), "num": e.to_number().to_bits() }),
                err,
                vec![],
            )
        }
        "vec_compare" => {
            let us = |v: &Value| -> Vec<u64> {
                v.as_array()
                    .map(|x| x.iter().map(u).collect())
                    .unwrap_or_default()
            };
            let is = |v: &Value| -> Vec<i64> {
                v.as_array()
                    .map(|x| x.iter().map(i).collect())
                    .unwrap_or_default()
            };
            let kind = a[0].as_str().unwrap();
            let n = a[1].as_array().map_or(0, |x| x.len());
            let mut res = vec![0i64; n];
            match kind {
                "UU" => compare::vec_compare_uu(&us(&a[1]), &us(&a[2]), &mut res),
                "II" => compare::vec_compare_ii(&is(&a[1]), &is(&a[2]), &mut res),
                "UI" => compare::vec_compare_ui(&us(&a[1]), &is(&a[2]), &mut res),
                _ => compare::vec_compare_iu(&is(&a[1]), &us(&a[2]), &mut res),
            }
            // Go's make([]int64, 0) encodes as [] (not null).
            (json!(res), None, vec![])
        }
        "compare_int" => nw((
            json!(compare::compare_int(
                i(&a[0]),
                a[1].as_bool().unwrap(),
                i(&a[2]),
                a[3].as_bool().unwrap()
            )),
            None,
        )),
        "compare_string" => nw((
            json!(compare::compare_string(
                &s(&a[0]),
                &s(&a[1]),
                a[2].as_str().unwrap()
            )),
            None,
        )),
        "type_preds" => {
            let tp = u(&a[0]) as u8;
            nw((
                json!([
                    etc::is_type_varchar(tp),
                    etc::is_type_unspecified(tp),
                    etc::is_type_prefixable(tp),
                    etc::is_type_fractionable(tp),
                    etc::is_type_time(tp),
                    etc::is_type_float(tp),
                    etc::is_type_integer(tp),
                    etc::is_type_numeric(tp),
                    etc::is_temporal_with_date(tp),
                    etc::is_string(tp),
                    etc::is_type_temporal(tp)
                ]),
                None,
            ))
        }
        "ft_preds" => {
            let mut ft = FieldType::new(u(&a[0]) as u8);
            ft.set_collate(a[1].as_str().unwrap());
            nw((
                json!([
                    etc::is_type_bit(&ft),
                    etc::is_binary_str(&ft),
                    etc::is_non_binary_str(&ft)
                ]),
                None,
            ))
        }
        "kind" => {
            let k = u(&a[0]) as u8;
            nw((
                json!({ "is_string": etc::is_string_kind(k), "str": etc::kind_str(k) }),
                None,
            ))
        }
        "encoding" => nw((encoding(a), None)),
        _ => panic!("unknown op {op}"),
    }
}

fn encoding(a: &[Value]) -> Value {
    let cs = a[0].as_str().unwrap();
    let src = unhex(a[1].as_str().unwrap());
    let e = enc::find_encoding(cs);
    let ops: [(&str, enc::Op); 8] = [
        ("replace_noerr", enc::OpReplaceNoErr),
        ("replace", enc::OpReplace),
        ("encode", enc::OpEncode),
        ("encode_noerr", enc::OpEncodeNoErr),
        ("encode_replace", enc::OpEncodeReplace),
        ("decode", enc::OpDecode),
        ("decode_noerr", enc::OpDecodeNoErr),
        ("decode_replace", enc::OpDecodeReplace),
    ];
    let mut tr = serde_json::Map::new();
    let mut fe = serde_json::Map::new();
    for (name, op) in ops {
        let (b, err) = e.transform(&src, op);
        let em = match err {
            Ok(()) => Value::Null,
            Err(x) => gs(&x.error()),
        };
        tr.insert(name.to_owned(), json!({ "b": hex(&b), "err": em }));
        let mut steps: Vec<String> = Vec::new();
        e.foreach(&src, op, &mut |from: &[u8], to: &[u8], ok: bool| {
            steps.push(format!("{}/{}/{}", hex(from), hex(to), ok));
            steps.len() < 64
        });
        fe.insert(
            name.to_owned(),
            if steps.is_empty() {
                Value::Null
            } else {
                json!(steps)
            },
        );
    }
    json!({
        "supported": enc::is_supported_encoding(cs),
        "noop_name": enc::find_encoding_take_utf8_as_noop(cs).name(),
        "name": e.name(), "tp": e.tp(),
        "peek": hex(e.peek(&src)),
        "mb_len": e.mb_len(&src),
        "valid": e.is_valid(&src),
        "cvb": enc::count_valid_bytes(e, &src),
        "cvbd": enc::count_valid_bytes_decode(e, &src),
        "upper": hex(&e.to_upper(&src)),
        "lower": hex(&e.to_lower(&src)),
        "transform": tr,
        "foreach": fe,
    })
}

fn check(rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| exec(op, args)));
    if want_err["name"] == "panic" {
        return match got {
            Err(_) => None,
            Ok((out, err, _)) => Some(format!("Go panics, Rust returned {out} / {err:?}")),
        };
    }
    let (out, err, warns) = match got {
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
    // Errors whose value Go discards (0 with the error) are compared on the error only.
    let want_out = &rec["out"];
    if out != *want_out && !(err.is_some() && out == json!(0) && want_out.is_number()) {
        return Some(format!("out: got {out}, want {want_out}"));
    }
    let got_err = err.as_ref().map(err_json);
    let want_err_cmp = if want_err.is_null() {
        None
    } else {
        Some(
            json!({ "code": want_err.get("code").cloned().unwrap_or(json!(0)), "msg": want_err["msg"] }),
        )
    };
    if got_err != want_err_cmp {
        return Some(format!("err: got {got_err:?}, want {want_err_cmp:?}"));
    }
    let got_w: Vec<Value> = warns.iter().map(err_json).collect();
    let want_w: Vec<Value> = rec
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|ws| {
            ws.iter()
                .map(|w| json!({ "code": w.get("code").cloned().unwrap_or(json!(0)), "msg": w["msg"] }))
                .collect()
        })
        .unwrap_or_default();
    if got_w != want_w {
        return Some(format!("warnings: got {got_w:?}, want {want_w:?}"));
    }
    None
}

#[test]
fn govec_datum() {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "datum");
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&rec) {
            fails.push(format!("{line}\n    {why}"));
        }
    }
    std::panic::set_hook(prev);
    let show = std::env::var("GOVEC_SHOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let shown: Vec<_> = fails.iter().take(show).collect();
    assert!(
        fails.is_empty(),
        "{} of {n} records differ:\n{}",
        fails.len(),
        shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
}

/// Part 2 of block D01.1: datum.go, field_type*.go, field_name.go, datum_eval.go.
mod part2 {
    use super::*;
    use std::borrow::Cow;
    use tidb_parser::model::CIStr;
    use tidb_types::core_time::CoreTime;
    use tidb_types::datum::{self as dm, Datum, GoValue};
    use tidb_types::datum_eval::compute_plus;
    use tidb_types::enum_type::Enum;
    use tidb_types::field_name::{EmptyName, FieldName, shallow};
    use tidb_types::field_type as ftm;
    use tidb_types::field_type_builder::new_field_type_builder;
    use tidb_types::json::{JSONValue, create_binary_json};
    use tidb_types::set_type::Set;
    use tidb_types::vector::parse_vector_float32;

    fn hex_opt(v: &Value) -> Option<Vec<u8>> {
        if v.is_null() {
            None
        } else {
            Some(unhex(v.as_str().unwrap()))
        }
    }

    fn st(v: &Value) -> String {
        v.as_str().unwrap().to_owned()
    }

    fn time_of_raw(raw: u64) -> Time {
        Time::from_raw(CoreTime(raw))
    }

    pub fn datum_in(v: &Value) -> Datum {
        let p = v.as_array().unwrap();
        let k = p[0].as_u64().unwrap() as u8;
        let mut d = Datum::default();
        match k {
            dm::KindNull => d.set_null(),
            dm::KindMinNotNull => d.set_min_not_null(),
            dm::KindMaxValue => d = dm::max_value_datum(),
            dm::KindInt64 => d.set_int64(i(&p[1])),
            dm::KindUint64 => d.set_uint64(u(&p[1])),
            dm::KindFloat32 => d.set_float32(f(&p[1]) as f32),
            dm::KindFloat64 => d.set_float64(f(&p[1])),
            dm::KindString => d.set_string(s(&p[1]), st(&p[2])),
            dm::KindBytes => d.set_bytes(hex_opt(&p[1])),
            dm::KindBinaryLiteral => {
                d.set_binary_literal(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default()))
            }
            dm::KindMysqlBit => d.set_mysql_bit(BinaryLiteral(hex_opt(&p[1]).unwrap_or_default())),
            dm::KindMysqlDecimal => {
                let mut dec = MyDecimal::default();
                let _ = dec.from_string(p[1].as_str().unwrap().as_bytes());
                d.set_mysql_decimal(dec);
            }
            dm::KindMysqlDuration => d.set_mysql_duration(Duration {
                duration: i(&p[1]),
                fsp: i(&p[2]) as isize,
            }),
            dm::KindMysqlEnum => d.set_mysql_enum(
                Enum {
                    value: u(&p[1]),
                    name: s(&p[2]),
                },
                st(&p[3]),
            ),
            dm::KindMysqlSet => d.set_mysql_set(
                Set {
                    value: u(&p[1]),
                    name: s(&p[2]),
                },
                st(&p[3]),
            ),
            dm::KindMysqlTime => d.set_mysql_time(time_of_raw(u(&p[1]))),
            dm::KindRaw => d.set_raw(hex_opt(&p[1])),
            dm::KindMysqlJSON => d.set_mysql_json(BinaryJSON::owned(
                u(&p[1]) as u8,
                hex_opt(&p[2]).unwrap_or_default(),
            )),
            dm::KindVectorFloat32 => d.set_vector_float32(
                parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap(),
            ),
            _ => panic!("datum_in: kind {k}"),
        }
        d
    }

    pub fn datum_out(d: &Datum) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("k".into(), json!(d.kind()));
        m.insert("frac".into(), json!(d.frac()));
        m.insert("len".into(), json!(d.length()));
        m.insert("i".into(), json!(d.get_int64()));
        m.insert("coll".into(), gs(d.collation().as_bytes()));
        m.insert("b".into(), json!(hex(d.get_bytes())));
        if d.kind() == dm::KindRaw && d.raw_b().is_none() {
            m.insert("raw_nil".into(), json!(true));
        }
        match d.kind() {
            dm::KindMysqlDecimal => {
                m.insert("dec".into(), dec_out(d.get_mysql_decimal()));
            }
            dm::KindMysqlTime => {
                m.insert("time".into(), time_out(&d.get_mysql_time()));
            }
            _ => {}
        }
        Value::Object(m)
    }

    fn datum_outs(ds: &[Datum]) -> Value {
        Value::Array(ds.iter().map(datum_out).collect())
    }

    fn ft_in(v: &Value) -> FieldType {
        let p = v.as_array().unwrap();
        let mut ft = new_field_type_builder()
            .set_type(u(&p[0]) as u8)
            .set_flag(u(&p[1]) as usize)
            .set_flen(i(&p[2]) as isize)
            .set_decimal(i(&p[3]) as isize)
            .set_charset(p[4].as_str().unwrap())
            .set_collate(p[5].as_str().unwrap())
            .build_p();
        if !p[6].is_null() {
            ft.set_elems(Some(elems(&p[6])));
        }
        ft
    }

    fn ft_out(ft: &FieldType) -> Value {
        json!({
            "tp": ft.get_type(), "flag": ft.get_flag(), "flen": ft.get_flen(), "dec": ft.get_decimal(),
            "cs": ft.get_charset(), "co": ft.get_collate(),
            "elems": ft.get_elems().map_or(Value::Null, |e| Value::Array(e.iter().map(|x| gs(x)).collect())),
        })
    }

    fn go_value_in(v: &Value) -> GoValue {
        let p = v.as_array().unwrap();
        match p[0].as_str().unwrap() {
            "nil" => GoValue::Nil,
            "bool" => GoValue::Bool(p[1].as_bool().unwrap()),
            "int" => GoValue::Int(i(&p[1])),
            "int64" => GoValue::Int64(i(&p[1])),
            "uint64" => GoValue::Uint64(u(&p[1])),
            "float32" => GoValue::Float32(f(&p[1]) as f32),
            "float64" => GoValue::Float64(f(&p[1])),
            "string" => GoValue::String(s(&p[1])),
            "bytes" => GoValue::Bytes(hex_opt(&p[1])),
            "decimal" => {
                let mut dec = MyDecimal::default();
                let _ = dec.from_string(p[1].as_str().unwrap().as_bytes());
                GoValue::Decimal(Box::new(dec))
            }
            "duration" => GoValue::Duration(Duration {
                duration: i(&p[1]),
                fsp: i(&p[2]) as isize,
            }),
            "enum" => GoValue::Enum(Enum {
                value: u(&p[1]),
                name: s(&p[2]),
            }),
            "set" => GoValue::Set(Set {
                value: u(&p[1]),
                name: s(&p[2]),
            }),
            "binlit" => GoValue::BinaryLiteral(BinaryLiteral(unhex(p[1].as_str().unwrap()))),
            "bitlit" => GoValue::BitLiteral(bl::BitLiteral(unhex(p[1].as_str().unwrap()))),
            "hexlit" => GoValue::HexLiteral(bl::HexLiteral(unhex(p[1].as_str().unwrap()))),
            "json" => GoValue::BinaryJSON(BinaryJSON::owned(
                u(&p[1]) as u8,
                unhex(p[2].as_str().unwrap()),
            )),
            "time" => GoValue::Time(time_of_raw(u(&p[1]))),
            "vector" => GoValue::VectorFloat32(
                parse_vector_float32(p[1].as_str().unwrap().as_bytes()).unwrap(),
            ),
            "slice" => GoValue::Slice(p[1].as_array().unwrap().iter().map(go_value_in).collect()),
            t => panic!("go_value_in {t}"),
        }
    }

    fn opt_err(r: Result<(), Error>) -> Option<Error> {
        r.err()
    }

    pub fn exec(op: &str, a: &[Value]) -> Option<Got> {
        let ww = |v: Value, e: Option<Error>, w: Arc<Warns>| -> Got {
            let ws = w.0.lock().unwrap().clone();
            (v, e, ws)
        };
        Some(match op {
            "d_convert" => {
                let (c, w) = ctx(a, 0);
                let mut d = datum_in(&a[1]);
                let ft = ft_in(&a[2]);
                let (ret, err) = d.convert_to(&c, &ft);
                ww(datum_out(&ret), opt_err(err), w)
            }
            "d_convert_recv" => {
                let (c, w) = ctx(a, 0);
                let mut d = datum_in(&a[1]);
                let ft = ft_in(&a[2]);
                let (ret, err) = d.convert_to(&c, &ft);
                ww(
                    json!({"ret": datum_out(&ret), "recv": datum_out(&d)}),
                    opt_err(err),
                    w,
                )
            }
            "d_compare" => {
                let (c, w) = ctx(a, 0);
                let x = datum_in(&a[1]);
                let y = datum_in(&a[2]);
                let (cmp, err) = x.compare(&c, &y, collate::get_collator(a[3].as_str().unwrap()));
                let eq = x.equals(Some(&y));
                ww(json!({"cmp": cmp, "eq": eq}), opt_err(err), w)
            }
            "d_to" => {
                let (c, w) = ctx(a, 0);
                let d = datum_in(&a[1]);
                let (v, e): (Value, Option<Error>) = match a[2].as_str().unwrap() {
                    "bool" => {
                        let (v, e) = d.to_bool(&c);
                        (json!(v), e.err())
                    }
                    "decimal" => {
                        let (v, e) = d.to_decimal(&c);
                        // Go: a nil *MyDecimal (ConvertJSONToDecimal error) records out = null.
                        if e.is_err() && d.kind() == dm::KindMysqlJSON {
                            (Value::Null, e.err())
                        } else {
                            (dec_out(&v), e.err())
                        }
                    }
                    "int64" => {
                        let (v, e) = d.to_int64(&c);
                        (json!(v), e.err())
                    }
                    "float64" => {
                        let (v, e) = d.to_float64(&c);
                        (json!(v.to_bits()), e.err())
                    }
                    "string" => match d.to_string() {
                        Ok(v) => (gs(&v), None),
                        Err(e) => (json!(""), Some(e)),
                    },
                    "bytes" => match d.to_bytes() {
                        Ok(v) => (json!(hex(&v)), None),
                        Err(e) => (json!(""), Some(e)),
                    },
                    "hash_key" => match d.to_hash_key() {
                        Ok(v) => (json!(hex(&v)), None),
                        Err(e) => (json!(""), Some(e)),
                    },
                    "json" => match d.to_mysql_json() {
                        Ok(j) => (json!([j.type_code, hex(&j.value)]), None),
                        Err(e) => (json!([0, ""]), Some(e)),
                    },
                    "str" => (gs(&d.string()), None),
                    "trunc" => (gs(&d.truncated_stringify()), None),
                    "value_str" => match convert::to_string(&d.get_value()) {
                        Ok(v) => (gs(&v), None),
                        Err(e) => (json!(""), Some(e)),
                    },
                    "mem" => {
                        let raw = d.raw_b();
                        let cap = raw.map_or(0, |_| {
                            d.mem_usage() - dm::EmptyDatumSize - d.collation().len() as i64
                        });
                        let len = raw.map_or(0, |b| b.len()) as i64;
                        (
                            json!({"mem": d.mem_usage() - cap + len, "est": d.estimated_mem_usage()}),
                            None,
                        )
                    }
                    "marshal" => match d.marshal_json() {
                        Ok(v) => (gs(&v), None),
                        Err(e) => (json!(""), Some(e)),
                    },
                    "lit4cmp" => (json!(hex(&d.get_binary_literal4_cmp().0)), None),
                    "encoded" => (gs(&d.get_binary_string_encoded()), None),
                    "decoded" => {
                        let (v, e) = d.get_binary_string_decoded(c.flags(), a[3].as_str().unwrap());
                        (gs(&v), e.err())
                    }
                    "checked" => {
                        let (v, e) = d.get_string_with_check(c.flags(), a[3].as_str().unwrap());
                        (gs(&v), e.err())
                    }
                    "clone" => {
                        let cl = d.clone_datum();
                        (
                            json!({"d": datum_out(&cl), "eq": cl.equals(Some(&d))}),
                            None,
                        )
                    }
                    other => panic!("d_to {other}"),
                };
                ww(v, e, w)
            }
            "d_unmarshal" => {
                let mut d = Datum::default();
                match d.unmarshal_json(&s(&a[0])) {
                    Ok(()) => (datum_out(&d), None, vec![]),
                    Err(e) => (Value::Null, Some(e), vec![]),
                }
            }
            "d_list" => {
                let (c, w) = ctx(a, 1);
                let mut ds: Vec<Datum> = a[2]
                    .as_array()
                    .map_or(vec![], |l| l.iter().map(datum_in).collect());
                match a[0].as_str().unwrap() {
                    "to_string" => match dm::datums_to_string(&ds, a[3].as_bool().unwrap()) {
                        Ok(v) => (gs(&v), None, vec![]),
                        Err(e) => (json!(""), Some(e), vec![]),
                    },
                    "to_string_smart" => {
                        match dm::datums_to_string_smart(&ds, a[3].as_bool().unwrap()) {
                            Ok(v) => (gs(&v), None, vec![]),
                            Err(e) => (json!(""), Some(e), vec![]),
                        }
                    }
                    "no_err" => (
                        json!({"s": gs(&dm::datums_to_str_no_err(&ds)), "smart": gs(&dm::datums_to_str_no_err_smart(&ds))}),
                        None,
                        vec![],
                    ),
                    "sort" => {
                        let err = dm::sort_datums(&c, &mut ds);
                        ww(datum_outs(&ds), err.err(), w)
                    }
                    "misc" => {
                        let cl = dm::clone_row(&ds);
                        (
                            json!({"clone": datum_outs(&cl), "null": dm::datums_contain_null(&ds),
                                   "est": dm::estimated_mem_usage(&ds, i(&a[3]) as isize)}),
                            None,
                            vec![],
                        )
                    }
                    other => panic!("d_list {other}"),
                }
            }
            "d_bound" => {
                let ft = ft_in(&a[0]);
                (
                    json!({"max": datum_out(&dm::get_max_value(&ft)), "min": datum_out(&dm::get_min_value(&ft))}),
                    None,
                    vec![],
                )
            }
            "d_plus" => {
                let x = datum_in(&a[0]);
                let y = datum_in(&a[1]);
                let (ret, err) = compute_plus(&x, &y);
                (datum_out(&ret), err.err(), vec![])
            }
            "d_new" => {
                let v = go_value_in(&a[0]);
                let d = dm::new_datum(v.clone());
                let mut d2 = Datum::default();
                let ft = ft_in(&a[1]);
                d2.set_value(v.clone(), &ft);
                let mut tp = FieldType::default();
                ftm::infer_param_type_from_datum(&d, &mut tp);
                let mut tp2 = FieldType::default();
                ftm::default_type_for_value(&v, &mut tp2, "utf8mb4", "utf8mb4_bin");
                let (vs, verr) = match convert::to_string(&v) {
                    Ok(x) => (x, Value::Null),
                    Err(e) => (Vec::new(), gs(&e.error())),
                };
                (
                    json!({"d": datum_out(&d), "d2": datum_out(&d2), "infer": ft_out(&tp), "def": ft_out(&tp2),
                           "str": gs(&d.string()), "to_string": gs(&vs), "to_string_err": verr}),
                    None,
                    vec![],
                )
            }
            "d_ctor" => {
                let n = i(&a[0]);
                let sv = s(&a[1]);
                let coll: Cow<'static, str> = Cow::Owned(st(&a[2]));
                let fl = f(&a[3]);
                let mut m = serde_json::Map::new();
                m.insert("uint".into(), datum_out(&dm::new_uint_datum(n as u64)));
                m.insert("bytes".into(), datum_out(&dm::new_bytes_datum(sv.clone())));
                m.insert(
                    "string".into(),
                    datum_out(&dm::new_string_datum(sv.clone())),
                );
                m.insert(
                    "cstring".into(),
                    datum_out(&dm::new_collation_string_datum(sv.clone(), coll.clone())),
                );
                m.insert("f64".into(), datum_out(&dm::new_float64_datum(fl)));
                m.insert("f32".into(), datum_out(&dm::new_float32_datum(fl as f32)));
                m.insert(
                    "dur".into(),
                    datum_out(&dm::new_duration_datum(Duration {
                        duration: n,
                        fsp: (n & 7) as isize,
                    })),
                );
                m.insert(
                    "time".into(),
                    datum_out(&dm::new_time_datum(time_of_raw(n as u64))),
                );
                let mut dec = MyDecimal::default();
                dec.from_int(n);
                m.insert("dec".into(), datum_out(&dm::new_decimal_datum(dec)));
                m.insert(
                    "json".into(),
                    datum_out(&dm::new_json_datum(create_binary_json(JSONValue::String(
                        sv.clone(),
                    )))),
                );
                m.insert(
                    "binlit".into(),
                    datum_out(&dm::new_binary_literal_datum(BinaryLiteral(sv.clone()))),
                );
                m.insert(
                    "bit".into(),
                    datum_out(&dm::new_mysql_bit_datum(BinaryLiteral(sv.clone()))),
                );
                m.insert(
                    "enum".into(),
                    datum_out(&dm::new_mysql_enum_datum(Enum {
                        name: sv.clone(),
                        value: n as u64,
                    })),
                );
                m.insert(
                    "cenum".into(),
                    datum_out(&dm::new_collate_mysql_enum_datum(
                        Enum {
                            name: sv.clone(),
                            value: n as u64,
                        },
                        coll.clone(),
                    )),
                );
                m.insert(
                    "set".into(),
                    datum_out(&dm::new_mysql_set_datum(
                        Set {
                            name: sv.clone(),
                            value: n as u64,
                        },
                        coll.clone(),
                    )),
                );
                m.insert("minnn".into(), datum_out(&dm::min_not_null_datum()));
                m.insert("max".into(), datum_out(&dm::max_value_datum()));
                let mut d = Datum::default();
                d.set_auto_id(n, u(&a[4]) as usize);
                m.insert("autoid".into(), datum_out(&d));
                d.set_float32_from_f64(fl);
                m.insert("f32f64".into(), datum_out(&d));
                d.set_bytes_as_string(sv.clone(), coll.clone(), n as u32);
                m.insert("bas".into(), datum_out(&d));
                d.set_raw(sv.clone());
                m.insert("raw".into(), datum_out(&d));
                d.set_collation(coll.clone());
                d.set_frac(n as isize);
                d.set_length(n as isize);
                m.insert("setters".into(), datum_out(&d));
                d.set_min_not_null();
                m.insert("setminnn".into(), datum_out(&d));
                d.set_null();
                m.insert("setnull".into(), datum_out(&d));
                (Value::Object(m), None, vec![])
            }
            "ft" => match a[0].as_str().unwrap() {
                "agg" => {
                    let fts: Vec<FieldType> = a[1].as_array().unwrap().iter().map(ft_in).collect();
                    let refs: Vec<&FieldType> = fts.iter().collect();
                    let mut flag = u(&a[2]) as usize;
                    let et = ftm::aggregate_eval_type(&refs, &mut flag);
                    let agg = ftm::agg_field_type(&refs);
                    let mut fixed = agg.deep_copy();
                    ftm::try_to_fix_flen_of_datetime(&mut fixed);
                    (
                        json!({"agg": ft_out(&agg), "fixed": ft_out(&fixed), "et": et.0, "flag": flag}),
                        None,
                        vec![],
                    )
                }
                "new" => {
                    let tp = u(&a[1]) as u8;
                    let (cs, co) = ftm::default_charset_for_type(tp);
                    (
                        json!({"ft": ft_out(&ftm::new_field_type(tp)), "cs": cs, "co": co}),
                        None,
                        vec![],
                    )
                }
                "new_coll" => (
                    ft_out(&ftm::new_field_type_with_collation(
                        u(&a[1]) as u8,
                        a[2].as_str().unwrap(),
                        i(&a[3]) as isize,
                    )),
                    None,
                    vec![],
                ),
                "modify" => {
                    let (can, err) =
                        ftm::check_modify_type_compatible(&ft_in(&a[1]), &ft_in(&a[2]));
                    (json!(can), err.err(), vec![])
                }
                "char_varchar" => (
                    json!(ftm::convert_between_char_and_varchar(
                        u(&a[1]) as u8,
                        u(&a[2]) as u8
                    )),
                    None,
                    vec![],
                ),
                "varchar_big" => (
                    Value::Null,
                    ftm::is_varchar_too_big_field_length(
                        i(&a[1]) as isize,
                        &s(&a[2]),
                        a[3].as_str().unwrap(),
                    )
                    .err(),
                    vec![],
                ),
                "set_flag" => {
                    let mut flag = u(&a[1]) as usize;
                    ftm::set_type_flag(&mut flag, u(&a[2]) as usize, a[3].as_bool().unwrap());
                    (json!(flag), None, vec![])
                }
                other => panic!("ft {other}"),
            },
            "ft_builder" => {
                let b = new_field_type_builder()
                    .set_type(u(&a[0]) as u8)
                    .set_flag(u(&a[1]) as usize)
                    .add_flag(u(&a[2]) as usize)
                    .del_flag(u(&a[3]) as usize)
                    .set_flen(i(&a[4]) as isize)
                    .set_decimal(i(&a[5]) as isize)
                    .set_charset(a[6].as_str().unwrap())
                    .set_collate(a[7].as_str().unwrap());
                let v = json!({"tp": b.get_type(), "flag": b.get_flag(), "flen": b.get_flen(),
                    "dec": b.get_decimal(), "cs": b.get_charset(), "co": b.get_collate()});
                let mut v = v;
                v["ft"] = ft_out(&b.build_p());
                (v, None, vec![])
            }
            "context" => {
                let fl = Flags(u(&a[0]) as u16);
                let on = a[1].as_bool().unwrap();
                let w = Arc::new(Warns::default());
                let c = new_context(fl, utc(), w.clone());
                let loc = tidb_base::gostd::time::load_location(a[2].as_str().unwrap()).unwrap();
                let c1 = c.with_flags(fl.with_truncate_as_warning(on));
                let c2 = c1.with_location(loc);
                c2.append_warning(tidb_types::errors::ErrTruncated.clone());
                let get = json!({"anu": fl.allow_negative_to_unsigned(), "cty": fl.cast_time_to_year_through_concat(),
                    "iid": fl.ignore_invalid_date_err(), "ite": fl.ignore_truncate_err(), "izd": fl.ignore_zero_date_err(),
                    "izi": fl.ignore_zero_in_date(), "sa": fl.skip_ascii_check(), "su": fl.skip_utf8_check(),
                    "su4": fl.skip_utf8mb4_check(), "taw": fl.truncate_as_warning()});
                let with = json!({"anu": fl.with_allow_negative_to_unsigned(on).0,
                    "cty": fl.with_cast_time_to_year_through_concat(on).0, "iid": fl.with_ignore_invalid_date_err(on).0,
                    "ite": fl.with_ignore_truncate_err(on).0, "izd": fl.with_ignore_zero_date_err(on).0,
                    "izi": fl.with_ignore_zero_in_date(on).0, "sa": fl.with_skip_sacii_check(on).0,
                    "su": fl.with_skip_utf8_check(on).0, "su4": fl.with_skip_utf8mb4_check(on).0,
                    "taw": fl.with_truncate_as_warning(on).0});
                let n = w.0.lock().unwrap().len();
                (
                    json!({"get": get, "with": with, "flags": c2.flags().0, "orig": c.flags().0,
                        "loc": c2.location().string(), "loc0": c.location().string(), "warns": n}),
                    None,
                    vec![],
                )
            }
            "field_name" => {
                let ci = |k: usize| CIStr::new(a[k].as_str().unwrap());
                let n = FieldName {
                    orig_tbl_name: ci(0),
                    orig_col_name: ci(1),
                    db_name: ci(2),
                    tbl_name: ci(3),
                    col_name: ci(4),
                    hidden: a[5].as_bool().unwrap(),
                    ..FieldName::default()
                };
                let c = n.clone_name();
                let sl = shallow(&[Arc::new(n.clone()), EmptyName.clone()]);
                (
                    json!({"s": n.string(), "mem": n.memory_usage(), "clone_s": c.string(),
                        "empty": sl[1].string(), "len": sl.len()}),
                    None,
                    vec![],
                )
            }
            _ => return None,
        })
    }
}

#[test]
fn datum_size() {
    // Go: unsafe.Sizeof(Datum{}) == 72.
    println!(
        "size_of::<Datum>() = {}",
        std::mem::size_of::<tidb_types::datum::Datum>()
    );
    assert!(std::mem::size_of::<tidb_types::datum::Datum>() <= 80);
}
