//! The TiKV oracle (block D01.8 spec §2, §4): the vendored TiKV encoders (`codec`,
//! `tidb_query_datatype::codec::datum`) and the Go-ported `tidb-codec`, compared byte for byte
//! in both directions:
//!
//! - encode: tidb-rs and TiKV produce the same bytes for the same value;
//! - decode: bytes TiKV writes (coprocessor responses, keys written by TiKV, decimals with
//!   the precision/frac TiKV computes) decode in tidb-rs to the same value, and bytes tidb-rs
//!   writes decode in TiKV to the original TiKV datum.
//!
//! A difference found here is an upstream TiDB/TiKV inconsistency (label `upstream-pitfall`),
//! not a porting error: the port follows Go (govec). Known differences are listed in
//! `KNOWN` and must still differ.

// Datum kinds keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::str::FromStr;

use codec::byte::{CompactByteEncoder, MemComparableByteCodec};
use codec::number::{NumberCodec, NumberEncoder};
use tidb_base::gostd::time::utc;
use tidb_codec as tc;
use tidb_query_datatype::codec::datum::{self as kd, Datum as KDatum, DatumDecoder};
use tidb_query_datatype::codec::mysql::{
    Decimal as KDecimal, Duration as KDuration, Json as KJson, Time as KTime, TimeType,
    VectorFloat32 as KVector,
};
use tidb_query_datatype::expr::EvalContext;
use tidb_types::MyDecimal;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::time::{Duration, Time};
use tidb_types::vector::parse_vector_float32;

fn ints() -> Vec<i64> {
    let mut v = vec![
        0,
        1,
        -1,
        239,
        240,
        255,
        256,
        -256,
        i64::MAX,
        i64::MIN,
        i64::MAX - 1,
        i64::MIN + 1,
        1 << 32,
        -(1 << 32),
        1 << 56,
        -(1 << 56),
    ];
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for _ in 0..500 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x as i64) >> (x % 64));
    }
    v
}

fn floats() -> Vec<f64> {
    let mut v = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        5e-324,
        -5e-324,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e300,
        -1e-300,
    ];
    v.extend(ints().iter().map(|&i| f64::from_bits(i as u64)).filter(|f| !f.is_nan()));
    v
}

fn byte_strings() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![vec![], vec![0], vec![1, 2, 3], vec![0xff; 8], vec![0; 9]];
    for n in 0..40usize {
        v.push((0..n).map(|i| (i * 37 + n) as u8).collect());
    }
    v
}

/// Primitive number and byte encodings, both directions.
#[test]
fn primitives_both_directions() {
    let mut diffs = vec![];
    for v in ints() {
        let mut t = Vec::new();
        tc::encode_int(&mut t, v);
        let mut k = [0u8; 8];
        NumberCodec::encode_i64(&mut k, v);
        if t != k {
            diffs.push(format!("EncodeInt({v})"));
        }
        assert_eq!(tc::decode_int(&k).unwrap().1, v, "DecodeInt(TiKV EncodeInt({v}))");

        let mut t = Vec::new();
        tc::encode_int_desc(&mut t, v);
        NumberCodec::encode_i64_desc(&mut k, v);
        if t != k {
            diffs.push(format!("EncodeIntDesc({v})"));
        }
        assert_eq!(tc::decode_int_desc(&k).unwrap().1, v);

        let u = v as u64;
        let mut t = Vec::new();
        tc::encode_uint(&mut t, u);
        NumberCodec::encode_u64(&mut k, u);
        if t != k {
            diffs.push(format!("EncodeUint({u})"));
        }
        assert_eq!(tc::decode_uint(&k).unwrap().1, u);
        let mut t = Vec::new();
        tc::encode_uint_desc(&mut t, u);
        NumberCodec::encode_u64_desc(&mut k, u);
        if t != k {
            diffs.push(format!("EncodeUintDesc({u})"));
        }
        assert_eq!(tc::decode_uint_desc(&k).unwrap().1, u);

        let mut t = Vec::new();
        tc::encode_varint(&mut t, v);
        let mut kv = [0u8; 10];
        let n = NumberCodec::encode_var_i64(&mut kv, v);
        if t != kv[..n] {
            diffs.push(format!("EncodeVarint({v})"));
        }
        assert_eq!(tc::decode_varint(&kv[..n]).unwrap().1, v);
        let (kvv, _) = NumberCodec::try_decode_var_i64(&t).unwrap();
        assert_eq!(kvv, v, "TiKV decodes tidb-rs EncodeVarint({v})");

        let mut t = Vec::new();
        tc::encode_uvarint(&mut t, u);
        let n = NumberCodec::encode_var_u64(&mut kv, u);
        if t != kv[..n] {
            diffs.push(format!("EncodeUvarint({u})"));
        }
        assert_eq!(tc::decode_uvarint(&kv[..n]).unwrap().1, u);
    }
    for f in floats() {
        let mut t = Vec::new();
        tc::encode_float(&mut t, f);
        let mut k = [0u8; 8];
        NumberCodec::encode_f64(&mut k, f);
        if t != k {
            diffs.push(format!("EncodeFloat({f:e})"));
        }
        assert_eq!(tc::decode_float(&k).unwrap().1.to_bits(), f.to_bits());
        let mut t = Vec::new();
        tc::encode_float_desc(&mut t, f);
        NumberCodec::encode_f64_desc(&mut k, f);
        if t != k {
            diffs.push(format!("EncodeFloatDesc({f:e})"));
        }
        assert_eq!(tc::decode_float_desc(&k).unwrap().1.to_bits(), f.to_bits());
        // TiKV decodes tidb-rs's bytes to the value (Go folds -0 into +0, see NEG_ZERO).
        let mut a = Vec::new();
        tc::encode_float(&mut a, f);
        let back = NumberCodec::decode_f64(&a);
        assert!(
            back.to_bits() == f.to_bits() || (f == 0.0 && back.to_bits() == 0),
            "TiKV decodes tidb-rs EncodeFloat({f:e}) to {back:e}"
        );
    }
    for b in byte_strings() {
        let mut t = Vec::new();
        tc::encode_bytes(&mut t, &b);
        let mut k = vec![0u8; MemComparableByteCodec::encoded_len(b.len())];
        MemComparableByteCodec::encode_all(&b, &mut k);
        if t != k {
            diffs.push(format!("EncodeBytes({b:?})"));
        }
        assert_eq!(tc::decode_bytes(&k, None).unwrap().1, b);
        let mut out = vec![0u8; t.len()];
        let (_, n) = MemComparableByteCodec::try_decode_first(&t, &mut out).unwrap();
        assert_eq!(&out[..n], &b[..], "TiKV decodes tidb-rs EncodeBytes");

        let mut t = Vec::new();
        tc::encode_bytes_desc(&mut t, &b);
        MemComparableByteCodec::encode_all_desc(&b, &mut k);
        if t != k {
            diffs.push(format!("EncodeBytesDesc({b:?})"));
        }
        assert_eq!(tc::decode_bytes_desc(&k, None).unwrap().1, b);

        let mut t = Vec::new();
        tc::encode_compact_bytes(&mut t, &b);
        let mut k: Vec<u8> = Vec::new();
        k.write_compact_bytes(&b).unwrap();
        if t != k {
            diffs.push(format!("EncodeCompactBytes({b:?})"));
        }
        assert_eq!(tc::decode_compact_bytes(&k).unwrap().1, &b[..]);
    }
    // NumberEncoder on a Vec: the writer TiKV's datum encoder uses.
    let mut w: Vec<u8> = Vec::new();
    w.write_i64(-5).unwrap();
    w.write_u8(9).unwrap();
    let mut t = Vec::new();
    tc::encode_int(&mut t, -5);
    t.push(9);
    assert_eq!(w, t);
    // -0.0: Go's `f >= 0` is true for -0, so EncodeFloat(-0) == EncodeFloat(+0); TiKV tests
    // the sign bit and encodes -0 below +0 (upstream-pitfall, see NEG_ZERO).
    let (known, other): (Vec<String>, Vec<String>) =
        diffs.into_iter().partition(|d| d.ends_with("(-0e0)"));
    assert!(other.is_empty(), "tidb-rs and TiKV differ: {other:#?}");
    assert!(!known.is_empty(), "the known -0.0 difference is gone: update NEG_ZERO");
}

/// The upstream difference this oracle found: Go TiDB encodes the float -0.0 like +0.0
/// (`encodeFloatToCmpUint64`: `f >= 0` is true for -0), TiKV keeps the sign
/// (`NumberCodec::encode_f64`: -0 -> 0x7fff_ffff_ffff_ffff). Keys and values of -0 written by
/// TiDB and by TiKV therefore differ, and -0 sorts below +0 only in TiKV's encoding. Both
/// decode the other's bytes without error (TiDB reads TiKV's -0 back as -0).
const NEG_ZERO: &str = "float -0e0";

/// A value as both datums.
struct Pair {
    name: String,
    tidb: Datum,
    tikv: KDatum,
}

fn pairs() -> Vec<Pair> {
    let mut ctx = EvalContext::default();
    let mut v = vec![];
    let mut add = |name: String, tidb: Datum, tikv: KDatum| v.push(Pair { name, tidb, tikv });
    add("null".into(), Datum::default(), KDatum::Null);
    add("min".into(), dm::min_not_null_datum(), KDatum::Min);
    add("max".into(), dm::max_value_datum(), KDatum::Max);
    for i in ints().into_iter().take(80) {
        add(format!("int {i}"), dm::new_int_datum(i), KDatum::I64(i));
        add(
            format!("uint {}", i as u64),
            dm::new_uint_datum(i as u64),
            KDatum::U64(i as u64),
        );
    }
    for f in floats().into_iter().take(40) {
        add(format!("float {f:e}"), dm::new_float64_datum(f), KDatum::F64(f));
    }
    for b in byte_strings() {
        add(
            format!("bytes {b:?}"),
            dm::new_bytes_datum(b.clone()),
            KDatum::Bytes(b),
        );
    }
    for s in [
        "0",
        "1",
        "-1",
        "0.1",
        "-0.000001",
        "123.456",
        "-123.4560",
        "99999999999999999999999999999999999.999999999999999999999999999999",
        "1e20",
        "100",
        "0.00",
        "-0",
        "12345678901234567890.1234567890",
    ] {
        let mut d = MyDecimal::default();
        let _ = d.from_string(s.as_bytes());
        add(
            format!("decimal {s}"),
            dm::new_decimal_datum(d),
            KDatum::Dec(KDecimal::from_str(s).unwrap()),
        );
    }
    for (n, fsp) in [
        (0i64, 0i8),
        (1_000_000_000, 0),
        (-1_500_000_000, 1),
        (3_723_000_500_000, 6),
        (-3_020_399_000_000_000, 0),
    ] {
        add(
            format!("duration {n}/{fsp}"),
            dm::new_duration_datum(Duration {
                duration: n,
                fsp: isize::from(fsp),
            }),
            KDatum::Dur(KDuration::from_nanos(n, fsp).unwrap()),
        );
    }
    for packed in [0u64, 1_840_814_726_643_384_320, 1_840_814_737_113_382_911, 1_829_018_087_510_785_536] {
        let mut t = tidb_types::time::new_time(CoreTime(0), tidb_base::mysql::TypeDatetime, 6);
        t.from_packed_uint(packed).unwrap();
        let k = KTime::from_packed_u64(&mut ctx, packed, TimeType::DateTime, 6).unwrap();
        add(
            format!("datetime {packed}"),
            dm::new_time_datum(t),
            KDatum::Time(k),
        );
    }
    for js in [
        "null",
        "true",
        "false",
        "0",
        "-1",
        "1.5",
        "\"abc\"",
        "[1,2,\"x\"]",
        "{\"a\":1,\"b\":[true,null]}",
        "18446744073709551615",
    ] {
        let t = parse_binary_json_from_string(js.as_bytes()).unwrap();
        add(
            format!("json {js}"),
            dm::new_json_datum(t),
            KDatum::Json(KJson::from_str(js).unwrap()),
        );
    }
    for (s, f) in [("[]", vec![]), ("[1]", vec![1.0f32]), ("[1.5,-2,0]", vec![1.5, -2.0, 0.0])] {
        add(
            format!("vector {s}"),
            dm::new_vector_float32_datum(parse_vector_float32(s.as_bytes()).unwrap()),
            KDatum::VectorFloat32(KVector::from_f32(f).unwrap()),
        );
    }
    v
}

/// Known Go/TiKV differences (upstream-pitfall entries): (pair name prefix, comparable).
/// Each must still differ, so a fix on either side shows up here.
const KNOWN: &[(&str, bool)] = &[(NEG_ZERO, true), (NEG_ZERO, false)];

/// Datum encodings, both directions.
#[test]
fn datums_both_directions() {
    tc::init();
    let loc = utc();
    let mut ctx = EvalContext::default();
    let mut diffs = vec![];
    let mut decode_diffs = vec![];
    for p in pairs() {
        for comparable in [true, false] {
            let mut t = Vec::new();
            let r = if comparable {
                tc::encode_key(&loc, &mut t, std::slice::from_ref(&p.tidb))
            } else {
                tc::encode_value(&loc, &mut t, std::slice::from_ref(&p.tidb))
            };
            r.unwrap_or_else(|e| panic!("{}: tidb-rs encode: {e}", p.name));
            let k = kd::encode(&mut ctx, std::slice::from_ref(&p.tikv), comparable)
                .unwrap_or_else(|e| panic!("{}: TiKV encode: {e:?}", p.name));
            let known = KNOWN
                .iter()
                .any(|(n, c)| p.name.starts_with(n) && *c == comparable);
            if (t != k) != known {
                diffs.push(format!(
                    "{} comparable={comparable}: tidb-rs {t:02x?} TiKV {k:02x?}",
                    p.name
                ));
            }
            // MinNotNull / MaxValue are range sentinels: a lone flag byte that only
            // DecodeRange accepts (and TiKV's datum decoder rejects 250 as well).
            if matches!(p.tikv, KDatum::Min | KDatum::Max) {
                continue;
            }
            // Decode direction 1: TiKV's bytes in tidb-rs.
            let (rem, d, err) = tc::decode_one(&k);
            if err.is_err() || !rem.is_empty() || !same_value(&d, &p.tidb) {
                decode_diffs.push(format!(
                    "{} comparable={comparable}: tidb-rs decodes TiKV bytes {k:02x?} to kind {} ({:?})",
                    p.name,
                    d.kind(),
                    err.err().map(|e| e.to_string())
                ));
            }
            // Decode direction 2: tidb-rs's bytes in TiKV (-0 comes back as +0, NEG_ZERO).
            let mut s: &[u8] = &t;
            match s.read_datum() {
                Ok(kdat) if kdat == p.tikv && s.is_empty() => {}
                Ok(KDatum::F64(x)) if known && x.to_bits() == 0 => {}
                // A time is stored as its packed uint: TiKV reads it back as U64.
                Ok(KDatum::U64(u))
                    if matches!(p.tikv, KDatum::Time(_))
                        && u == p.tidb.get_mysql_time().to_packed_uint().unwrap() => {}
                Ok(_) => decode_diffs.push(format!(
                    "{} comparable={comparable}: TiKV decodes tidb-rs bytes {t:02x?} to another value",
                    p.name
                )),
                Err(e) => decode_diffs.push(format!(
                    "{} comparable={comparable}: TiKV cannot decode tidb-rs bytes {t:02x?}: {e:?}",
                    p.name
                )),
            }
        }
    }
    assert!(diffs.is_empty(), "encodings differ: {diffs:#?}");
    assert!(decode_diffs.is_empty(), "decodings differ: {decode_diffs:#?}");
}

/// The value a decoded datum must have: decoding loses the kind of strings (Bytes), the
/// flavour of floats (Float64), times come back as the packed value with the type the
/// key does not store, durations with MaxFsp; decimals keep the value.
fn same_value(got: &Datum, want: &Datum) -> bool {
    use dm::*;
    match want.kind() {
        KindNull => got.kind() == KindNull,
        // MinNotNull encodes as a lone bytesFlag (not decodable alone); MaxValue as maxFlag.
        KindMinNotNull | KindMaxValue => true,
        KindInt64 => got.kind() == KindInt64 && got.get_int64() == want.get_int64(),
        KindUint64 => got.kind() == KindUint64 && got.get_uint64() == want.get_uint64(),
        KindFloat64 => {
            got.kind() == KindFloat64 && got.get_float64().to_bits() == want.get_float64().to_bits()
        }
        KindBytes => got.kind() == KindBytes && got.get_bytes() == want.get_bytes(),
        KindMysqlDecimal => {
            got.kind() == KindMysqlDecimal
                && got.get_mysql_decimal().compare(want.get_mysql_decimal()) == 0
        }
        KindMysqlDuration => {
            got.kind() == KindMysqlDuration
                && got.get_mysql_duration().duration == want.get_mysql_duration().duration
        }
        KindMysqlTime => {
            got.kind() == KindUint64
                && got.get_uint64() == want.get_mysql_time().to_packed_uint().unwrap()
        }
        KindMysqlJSON => {
            got.kind() == KindMysqlJSON
                && got.get_mysql_json().type_code == want.get_mysql_json().type_code
                && got.get_mysql_json().value == want.get_mysql_json().value
        }
        KindVectorFloat32 => {
            got.kind() == KindVectorFloat32
                && got.get_vector_float32().serialize_to(Vec::new())
                    == want.get_vector_float32().serialize_to(Vec::new())
        }
        _ => false,
    }
}

/// Decimals with TiKV-computed precision and frac (`prec_and_frac`), the form a coprocessor
/// response carries: tidb-rs decodes the value and the precision/frac bytes as written.
#[test]
fn tikv_decimals_decode() {
    let mut x: u64 = 0x2545f4914f6cdd1d;
    let mut n = 0;
    for _ in 0..3000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let int = (x % 1_000_000_000_000) as i64 - 500_000_000_000;
        let frac = x >> 40;
        let digits = (x % 12) as usize;
        let s = if digits == 0 {
            format!("{int}")
        } else {
            let f = format!("{frac:030}");
            format!("{int}.{}", &f[..digits])
        };
        let kdec = KDecimal::from_str(&s).unwrap();
        let (prec, fr) = kdec.prec_and_frac();
        let mut k: Vec<u8> = Vec::new();
        k.write_u8(6).unwrap();
        use tidb_query_datatype::codec::mysql::DecimalEncoder;
        k.write_decimal(&kdec, prec, fr).unwrap();
        let (rem, d, err) = tc::decode_one(&k);
        assert!(err.is_ok() && rem.is_empty(), "{s}: {err:?}");
        let mut want = MyDecimal::default();
        want.from_string(s.as_bytes()).unwrap();
        assert_eq!(d.get_mysql_decimal().compare(&want), 0, "{s}");
        assert_eq!((d.length(), d.frac()), (isize::from(prec), isize::from(fr)), "{s}");
        // And tidb-rs re-encodes the decoded datum (Length/Frac set) to TiKV's bytes.
        let mut t = Vec::new();
        tc::encode_value(&utc(), &mut t, std::slice::from_ref(&d)).unwrap();
        assert_eq!(t, k, "{s}: re-encode");
        n += 1;
    }
    assert_eq!(n, 3000);
}

/// Keys TiKV writes for a time compared with the ones tidb-rs writes (TiKV's packed time and
/// Go's packed time are the same layout).
#[test]
fn times_match() {
    let mut ctx = EvalContext::default();
    let mut x: u64 = 0x853c49e6748fea9b;
    for _ in 0..2000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // A valid datetime: year 1000..9999, month 1..12, day 1..28, h/m/s, micro.
        let (y, mo, d) = (1000 + x % 9000, 1 + (x >> 14) % 12, 1 + (x >> 18) % 28);
        let (h, mi, s, us) = ((x >> 23) % 24, (x >> 28) % 60, (x >> 34) % 60, (x >> 40) % 1_000_000);
        let ymd = ((y * 13 + mo) << 5) | d;
        let hms = (h << 12) | (mi << 6) | s;
        let packed = (((ymd << 17) | hms) << 24) | us;
        let k = KTime::from_packed_u64(&mut ctx, packed, TimeType::DateTime, 6).unwrap();
        let kp = k.to_packed_u64(&mut ctx).unwrap();
        let mut t = Time::from_raw(CoreTime(0));
        t = {
            let mut tt = tidb_types::time::new_time(CoreTime(0), tidb_base::mysql::TypeDatetime, 6);
            tt.from_packed_uint(packed).unwrap();
            let _ = &mut t;
            tt
        };
        assert_eq!(t.to_packed_uint().unwrap(), kp, "packed {packed}");
    }
}
