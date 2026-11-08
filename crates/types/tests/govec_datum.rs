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
    let shown: Vec<_> = fails.iter().take(30).collect();
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
