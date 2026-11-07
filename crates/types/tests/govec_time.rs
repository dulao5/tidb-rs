//! Replays the Go-generated vectors `tests/vectors/v8.5.8/<target>/time.jsonl.gz`
//! (tracker `govec`, domain `time`: Go TiDB v8.5.8 `pkg/types` time.go, core_time.go,
//! fsp.go and the Go `time` package behaviour they use) against the Rust port.
//!
//! Every record's `out` must be equal as JSON (outputs carry the raw Time word, the
//! string forms, Duration nanoseconds, time.Time Unix seconds/nanoseconds/zone name and
//! `String()`); errors and warnings must match by code and message; `err.name == "panic"`
//! records must panic in Rust too. The vectors start with one `zone` record per TZif
//! file; zone ids in later records refer to them (see govec `domains/time.go`).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use tidb_base::Error;
use tidb_base::contextutil::WarnAppender;
use tidb_base::gostd::time::{self as gotime, Location};
use tidb_types::MyDecimal;
use tidb_types::context::{Context, Flags, new_context};
use tidb_types::core_time::{self, CoreTime};
use tidb_types::fsp;
use tidb_types::time::{self as t, Duration, Time};

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
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/time.jsonl.gz",
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

/// A Go string as govec encodes it (`vec.GoString`): a JSON string, or `{"hex": ...}`.
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

struct Replay {
    zones: HashMap<String, Arc<Location>>,
}

/// (out, err, warnings)
type Got = (Value, Option<Error>, Vec<Error>);

fn time_out(t: &Time) -> Value {
    json!({ "raw": t.raw().0, "s": gs(&t.string()) })
}

fn dur_out(d: &Duration) -> Value {
    json!({ "d": d.duration, "fsp": d.fsp, "s": gs(&d.string()) })
}

fn go_time_out(t: &gotime::Time) -> Value {
    json!({ "unix": t.unix(), "nsec": t.nanosecond(), "loc": t.location().string(), "s": gs(t.string().as_bytes()) })
}

fn dur_of(v: &Value) -> Duration {
    Duration {
        duration: v["d"].as_i64().unwrap(),
        fsp: v["fsp"].as_i64().unwrap() as isize,
    }
}

fn raw(v: &Value) -> Time {
    Time::from_raw(CoreTime(v.as_u64().unwrap()))
}

fn ok<T>(r: Result<(), Error>, v: T) -> (T, Option<Error>) {
    (v, r.err())
}

impl Replay {
    fn zone(&self, id: &str) -> Arc<Location> {
        if id == "UTC" {
            return gotime::utc();
        }
        if let Some(rest) = id.strip_prefix("fixed:") {
            let i = rest.rfind(':').unwrap();
            return gotime::fixed_zone(&rest[..i], rest[i + 1..].parse().unwrap());
        }
        Arc::clone(
            self.zones
                .get(id)
                .unwrap_or_else(|| panic!("zone {id} not loaded")),
        )
    }

    fn ctx(&self, args: &[Value], i: usize) -> (Context, Arc<Warns>) {
        let w = Arc::new(Warns::default());
        let flags = Flags(args[i].as_u64().unwrap() as u16);
        let loc = self.zone(args[i + 1].as_str().unwrap());
        (new_context(flags, loc, w.clone()), w)
    }

    fn go_time(&self, v: &Value) -> gotime::Time {
        gotime::unix(v["unix"].as_i64().unwrap(), v["nsec"].as_i64().unwrap())
            .in_loc(&self.zone(v["zone"].as_str().unwrap()))
    }

    fn exec(&mut self, op: &str, a: &[Value]) -> Got {
        let s = |i: usize| ungs(&a[i]);
        let take = |w: &Arc<Warns>| std::mem::take(&mut *w.0.lock().unwrap());
        match op {
            "zone" => {
                let id = a[0].as_str().unwrap();
                // The data is in the record's out; exec_record loads it before calling here.
                let l = self.zone(id);
                (json!({ "name": l.string() }), None, vec![])
            }
            "zone_lookup" => {
                let tm = gotime::unix(a[1].as_i64().unwrap(), 0)
                    .in_loc(&self.zone(a[0].as_str().unwrap()));
                let tl = tm.location();
                let z = tl.lookup(tm.unix());
                let (start, end) = tm.zone_bounds();
                let bound = |b: &gotime::Time| {
                    if b.equal(&gotime::Time::zero()) {
                        Value::Null
                    } else {
                        json!(b.unix())
                    }
                };
                (
                    json!({ "name": z.name, "offset": z.offset, "start": bound(&start), "end": bound(&end), "t": go_time_out(&tm) }),
                    None,
                    vec![],
                )
            }
            "go_date" => {
                let v: Vec<i64> = a[0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_i64().unwrap())
                    .collect();
                let tm = gotime::date(
                    v[0],
                    v[1],
                    v[2],
                    v[3],
                    v[4],
                    v[5],
                    v[6],
                    &self.zone(a[1].as_str().unwrap()),
                );
                let (y, m, d) = tm.date();
                let (h, mi, sec) = tm.clock();
                (
                    json!({ "t": go_time_out(&tm), "date": [y, m, d, h, mi, sec, tm.nanosecond()], "wd": tm.weekday(), "yd": tm.year_day() }),
                    None,
                    vec![],
                )
            }
            "parse_time" | "parse_time_float" => {
                let (ctx, w) = self.ctx(a, 3);
                let tp = a[1].as_u64().unwrap() as u8;
                let fsp = a[2].as_i64().unwrap() as isize;
                let (tm, err) = if op == "parse_time" {
                    t::parse_time(&ctx, &s(0), tp, fsp)
                } else {
                    t::parse_time_from_float_string(&ctx, &s(0), tp, fsp)
                };
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_helper" => {
                let (ctx, w) = self.ctx(a, 2);
                let (tm, err) = match a[1].as_str().unwrap() {
                    "datetime" => t::parse_datetime_str(&ctx, &s(0)),
                    "timestamp" => t::parse_timestamp(&ctx, &s(0)),
                    _ => t::parse_date(&ctx, &s(0)),
                };
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_num" => {
                let (ctx, w) = self.ctx(a, 3);
                let (tm, err) = t::parse_time_from_num(
                    &ctx,
                    a[0].as_i64().unwrap(),
                    a[1].as_u64().unwrap() as u8,
                    a[2].as_i64().unwrap() as isize,
                );
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_num_helper" => {
                let (ctx, w) = self.ctx(a, 2);
                let n = a[0].as_i64().unwrap();
                let (tm, err) = match a[1].as_str().unwrap() {
                    "datetime" => t::parse_datetime_from_num(&ctx, n),
                    "timestamp" => t::parse_timestamp_from_num(&ctx, n),
                    _ => t::parse_date_from_num(&ctx, n),
                };
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_int64" => {
                let (ctx, w) = self.ctx(a, 1);
                let (tm, err) = t::parse_time_from_int64(&ctx, a[0].as_i64().unwrap());
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_float64" => {
                let (ctx, w) = self.ctx(a, 1);
                let f = match a[0].as_str().unwrap() {
                    "NaN" => f64::NAN,
                    "+Inf" => f64::INFINITY,
                    "-Inf" => f64::NEG_INFINITY,
                    x => x.parse().unwrap(),
                };
                let (tm, err) = t::parse_time_from_float64(&ctx, f);
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_decimal" => {
                let (ctx, w) = self.ctx(a, 1);
                let mut dec = MyDecimal::default();
                let _ = dec.from_string(&s(0));
                let (tm, err) = t::parse_time_from_decimal(&ctx, &dec);
                (time_out(&tm), err.err(), take(&w))
            }
            "parse_time_year" => match t::parse_time_from_year(a[0].as_i64().unwrap()) {
                Ok(tm) => (time_out(&tm), None, vec![]),
                Err(e) => (Value::Null, Some(e), vec![]),
            },
            "parse_duration" => {
                let (ctx, w) = self.ctx(a, 2);
                let (d, is_null, err) =
                    t::parse_duration(&ctx, &s(0), a[1].as_i64().unwrap() as isize);
                (
                    json!({ "d": dur_out(&d), "null": is_null }),
                    err.err(),
                    take(&w),
                )
            }
            "time_props" => {
                let tm = raw(&a[0]);
                let mut weeks = vec![];
                let mut yws = vec![];
                for m in 0..8 {
                    weeks.push(tm.week(m));
                    let (y, wk) = tm.year_week(m);
                    yws.push(json!([y, wk]));
                }
                let packed = tm.to_packed_uint().unwrap();
                let js = String::from_utf8(tm.marshal_json().unwrap()).unwrap();
                (
                    json!({
                        "core": tm.core_time().string(), "y": tm.year(), "mo": tm.month(), "d": tm.day(),
                        "h": tm.hour(), "mi": tm.minute(), "s": tm.second(), "us": tm.microsecond(),
                        "tp": tm.tp(), "fsp": tm.fsp(), "zero": tm.is_zero(), "inv0": tm.invalid_zero(),
                        "leap": tm.is_leap_year(), "yday": tm.year_day(), "wd": tm.weekday(),
                        "str": gs(&tm.string()), "week": weeks, "yweek": yws,
                        "num": tm.to_number().string(), "packed": packed, "json": js,
                    }),
                    None,
                    vec![],
                )
            }
            "date_format" => match raw(&a[0]).date_format(&s(1)) {
                Ok(v) => (gs(&v), None, vec![]),
                Err(e) => (gs(b""), Some(e), vec![]),
            },
            "from_packed" => {
                let mut tm = raw(&a[0]);
                let err = tm.from_packed_uint(a[1].as_u64().unwrap());
                (time_out(&tm), err.err(), vec![])
            }
            "unmarshal_json" => {
                let mut tm = raw(&a[0]);
                let err = tm.unmarshal_json(&s(1));
                (json!(tm.raw().0), err.err(), vec![])
            }
            "convert" => {
                let (ctx, w) = self.ctx(a, 2);
                let (tm, err) = raw(&a[0]).convert(&ctx, a[1].as_u64().unwrap() as u8);
                (time_out(&tm), err.err(), take(&w))
            }
            "round_frac" => {
                let (ctx, w) = self.ctx(a, 2);
                let (tm, err) = raw(&a[0]).round_frac(&ctx, a[1].as_i64().unwrap() as isize);
                (time_out(&tm), err.err(), take(&w))
            }
            "compare_string" => {
                let (ctx, w) = self.ctx(a, 2);
                match raw(&a[0]).compare_string(&ctx, &s(1)) {
                    Ok(r) => (json!(r), None, take(&w)),
                    Err(e) => (json!(0), Some(e), take(&w)),
                }
            }
            "time_sub" => {
                let (ctx, w) = self.ctx(a, 2);
                let (x, y) = (raw(&a[0]), raw(&a[1]));
                (
                    json!({ "d": dur_out(&x.sub(&ctx, &y)), "cmp": x.compare(&y) }),
                    None,
                    take(&w),
                )
            }
            "time_add" => {
                let (ctx, w) = self.ctx(a, 2);
                let (tm, err) = raw(&a[0]).add(&ctx, dur_of(&a[1]));
                (time_out(&tm), err.err(), take(&w))
            }
            "convert_tz" => {
                let mut tm = raw(&a[0]);
                let err = tm.convert_time_zone(
                    &self.zone(a[1].as_str().unwrap()),
                    &self.zone(a[2].as_str().unwrap()),
                );
                (time_out(&tm), err.err(), vec![])
            }
            "date_diff" => (
                json!(core_time::date_diff(
                    raw(&a[0]).core_time(),
                    raw(&a[1]).core_time()
                )),
                None,
                vec![],
            ),
            "timestamp_diff" => (
                json!(t::timestamp_diff_of(
                    a[0].as_str().unwrap().as_bytes(),
                    raw(&a[1]),
                    raw(&a[2])
                )),
                None,
                vec![],
            ),
            "time_from_days" => (
                time_out(&t::time_from_days(a[0].as_i64().unwrap())),
                None,
                vec![],
            ),
            "overflow" => {
                let (ctx, w) = self.ctx(a, 1);
                match t::date_time_is_overflow(&ctx, raw(&a[0])) {
                    Ok(r) => (json!(r), None, take(&w)),
                    Err(e) => (json!(false), Some(e), take(&w)),
                }
            }
            "str_to_date" => {
                let (ctx, w) = self.ctx(a, 3);
                let mut tm = raw(&a[0]);
                let ok = tm.str_to_date(&ctx, &s(1), &s(2));
                (json!({ "ok": ok, "t": time_out(&tm) }), None, take(&w))
            }
            "format_type" => {
                let (d, dt) = t::get_format_type(&s(0));
                (json!([d, dt]), None, vec![])
            }
            "date_split" => {
                let str = s(0);
                let (idx, sign, hour, sep, minute) = t::get_timezone(&str);
                let seps = match t::parse_date_format(&str) {
                    None => Value::Null,
                    Some(v) => Value::Array(v.iter().map(|p| gs(p)).collect()),
                };
                (
                    json!({
                        "tz": [idx, gs(sign), gs(hour), gs(sep), gs(minute)], "seps": seps,
                        "fsp": t::get_fsp(&str), "frac": t::get_frac_index(&str),
                        "is_date": t::is_date_format(&str), "date_fsp": t::date_fsp(&str),
                    }),
                    None,
                    vec![],
                )
            }
            "parse_year" => match t::parse_year(&s(0)) {
                Ok(y) => (json!(y), None, vec![]),
                Err(e) => (json!(0), Some(e), vec![]),
            },
            "adjust_year" => {
                let (y, err) =
                    t::adjust_year_checked(a[0].as_i64().unwrap(), a[1].as_bool().unwrap());
                let (y, e) = ok(err, y);
                (json!(y), e, vec![])
            }
            "go_time_ops" => {
                let g = self.go_time(&a[0]);
                let v: Vec<i64> = a[1]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_i64().unwrap())
                    .collect();
                let fsp = v[3] as isize;
                let (ad, aerr) = core_time::add_date(v[0], v[1], v[2], &g);
                let (r, rerr) = t::round_frac(&g, fsp);
                let (tr, terr) = t::truncate_frac(&g, fsp);
                let es = |e: Result<(), Error>| match e {
                    Ok(()) => Value::Null,
                    Err(e) => gs(&e.error()),
                };
                let from_go = t::new_time(t::from_go_time(&g), tidb_base::mysql::TypeDatetime, 6);
                (
                    json!({
                        "add": go_time_out(&ad), "add_err": es(aerr), "round": go_time_out(&r),
                        "round_err": es(rerr), "trunc": go_time_out(&tr), "trunc_err": es(terr),
                        "last_day": core_time::get_last_day(v[0] as isize, v[1] as isize),
                        "from_go": time_out(&from_go),
                    }),
                    None,
                    vec![],
                )
            }
            "dur_format" => match dur_of(&a[0]).duration_format(&s(1)) {
                Ok(v) => (gs(&v), None, vec![]),
                Err(e) => (gs(b""), Some(e), vec![]),
            },
            "dur_props" => {
                let d = dur_of(&a[0]);
                (
                    json!({
                        "h": d.hour(), "m": d.minute(), "s": d.second(), "us": d.micro_second(),
                        "num": d.to_number().string(), "neg": dur_out(&d.neg()), "str": gs(&d.string()),
                        "cmp": d.compare(t::new_duration(1, 2, 3, 4, 0)),
                        "max": dur_out(&t::max_mysql_duration(d.fsp)),
                    }),
                    None,
                    vec![],
                )
            }
            "dur_arith" => {
                let (x, y) = (dur_of(&a[0]), dur_of(&a[1]));
                let (sum, err) = match x.add(y) {
                    Ok(v) => (v, None),
                    Err(e) => (Duration::default(), Some(e)),
                };
                let mut res = json!({ "add": dur_out(&sum) });
                match x.sub(y) {
                    Ok(v) => res["sub"] = dur_out(&v),
                    Err(e) => {
                        res["sub"] = dur_out(&Duration::default());
                        res["sub_err"] = gs(&e.error());
                    }
                }
                (res, err, vec![])
            }
            "dur_compare_string" => {
                let (ctx, w) = self.ctx(a, 2);
                match dur_of(&a[0]).compare_string(&ctx, &s(1)) {
                    Ok(r) => (json!(r), None, take(&w)),
                    Err(e) => (json!(0), Some(e), take(&w)),
                }
            }
            "dur_to_time" => {
                let (ctx, w) = self.ctx(a, 3);
                let g = self.go_time(&a[2]);
                let (tm, err) = dur_of(&a[0]).convert_to_time_with_timestamp(
                    &ctx,
                    a[1].as_u64().unwrap() as u8,
                    &g,
                );
                (time_out(&tm), err.err(), take(&w))
            }
            "dur_to_year" => {
                let (ctx, w) = self.ctx(a, 2);
                let g = self.go_time(&a[1]);
                let (y, err) = dur_of(&a[0]).convert_to_year_from_now(&ctx, &g);
                (json!(y), err.err(), take(&w))
            }
            "extract_datetime_num" => match t::extract_datetime_num(&raw(&a[0]), &s(1)) {
                Ok(v) => (json!(v), None, vec![]),
                Err(e) => (json!(0), Some(e), vec![]),
            },
            "extract_duration_num" => match t::extract_duration_num(&dur_of(&a[0]), &s(1)) {
                Ok(v) => (json!(v), None, vec![]),
                Err(e) => (json!(0), Some(e), vec![]),
            },
            "parse_duration_value" => {
                let ((y, m, d, n, fsp), err) = t::parse_duration_value(&s(0), &s(1));
                (json!([y, m, d, n, fsp]), err.err(), vec![])
            }
            "extract_duration_value" => match t::extract_duration_value(&s(0), &s(1)) {
                Ok(d) => (dur_out(&d), None, vec![]),
                Err(e) => (dur_out(&t::ZeroDuration), Some(e), vec![]),
            },
            "unit_kind" => {
                let u = s(0);
                (
                    json!([
                        t::is_clock_unit(&u),
                        t::is_date_unit(&u),
                        t::is_microsecond_unit(&u)
                    ]),
                    None,
                    vec![],
                )
            }
            "from_date" => {
                let v: Vec<isize> = a[0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_i64().unwrap() as isize)
                    .collect();
                let (ct, okk) = t::from_date_checked(v[0], v[1], v[2], v[3], v[4], v[5], v[6]);
                let (f, err) = fsp::check_fsp(v[7]);
                let mut fspv = json!({ "check": f });
                if let Err(e) = err {
                    fspv["err"] = Value::String(String::from_utf8_lossy(&e.error()).into_owned());
                }
                (
                    json!({
                        "checked": ct.0, "ok": okk,
                        "raw": t::from_date(v[0], v[1], v[2], v[3], v[4], v[5], v[6]).0,
                        "fsp": fspv,
                    }),
                    None,
                    vec![],
                )
            }
            _ => panic!("unknown op {op}"),
        }
    }
}

fn check(rep: &mut Replay, rec: &Value) -> Option<String> {
    let op = rec["op"].as_str().unwrap();
    let args = rec["args"].as_array().unwrap();
    if op == "zone" {
        let id = args[0].as_str().unwrap();
        let (_, file) = id.split_once(':').unwrap();
        let data = ungs(&rec["out"]["data"]);
        let name = file.replace('_', "/");
        let loc = gotime::load_location_from_tzdata(&name, &data).expect("TZif data");
        rep.zones.insert(id.to_owned(), Arc::new(loc));
    }
    let want_err = &rec["err"];
    let got = catch_unwind(AssertUnwindSafe(|| rep.exec(op, args)));
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
    let mut want_out = rec["out"].clone();
    if op == "zone" {
        want_out = json!({ "name": rec["out"]["name"] });
    }
    if out != want_out {
        return Some(format!("out: got {out}, want {want_out}"));
    }
    let got_err = err.as_ref().map(err_json);
    // Go's encoding/json syntax error texts are not ported (as for MyDecimal.UnmarshalJSON):
    // a syntax error must be a code-less error, whatever its text.
    let json_syntax = op == "unmarshal_json"
        && want_err["name"] == "other"
        && !ungs(&want_err["msg"]).starts_with(b"json: cannot unmarshal");
    if json_syntax {
        return match &err {
            Some(e) if e.code() == 0 => None,
            _ => Some(format!("err: got {got_err:?}, want a JSON syntax error")),
        };
    }
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

/// The lookup cache of every TZif zone in the vectors answers like the search, for each
/// hour from two years before now to two years after (the cache holds the zone of "now").
#[test]
fn lookup_cache_matches_search() {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let now = gotime::now().unix();
    let mut zones = 0;
    for line in text.lines().skip(1) {
        let rec: Value = serde_json::from_str(line).unwrap();
        if rec["op"] != "zone" {
            break;
        }
        zones += 1;
        let id = rec["args"][0].as_str().unwrap();
        let name = id.split_once(':').unwrap().1.replace('_', "/");
        let l = gotime::load_location_from_tzdata(&name, &ungs(&rec["out"]["data"])).unwrap();
        let mut sec = now - 2 * 366 * 86400;
        while sec < now + 2 * 366 * 86400 {
            assert_eq!(l.lookup(sec), l.lookup_uncached(sec), "{id} at {sec}");
            sec += 3600;
        }
    }
    assert_eq!(zones, 18);
}

#[test]
fn govec_time() {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(vectors()).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "time");
    let mut rep = Replay {
        zones: HashMap::new(),
    };
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        if let Some(why) = check(&mut rep, &rec) {
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
