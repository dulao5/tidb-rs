//! The TiKV oracle (block D01.11 spec §2, owner decision tidb-rs#39): the vendored TiKV
//! table and row codecs (`tidb_query_datatype::codec::{table, row::v2}`) and the Go-ported
//! `tidb-tablecodec`, compared byte for byte in both directions:
//!
//! - keys: the record / index key layouts tidb-rs and TiKV build are the same bytes, and each
//!   side decodes the other's keys to the same table ID and handle;
//! - rows (v2): every Go-encoded row of the govec vectors that Go decodes without error is
//!   parsed by TiKV's `RowSlice` to the same column bytes, NULL set and checksum; TiKV's
//!   encoder, given the column values tidb-rs decoded, writes the row's exact bytes; TiKV's
//!   v2-to-v1 datum conversion (`write_v2_as_datum`, used by `cut_row`) produces the bytes of
//!   `rowcodec.BytesDecoder` (Go `encodeOldDatum`);
//! - rows (v1): TiKV's `cut_row` cuts Go's v1 rows to the column bytes `CutRowNew` returns.
//!
//! TiKV's index scan executor (index value layouts, restored data) is not vendored
//! (`tidb_query_executors`): the index values are checked against Go only (govec).
//!
//! A difference found here is an upstream TiDB/TiKV inconsistency (label `upstream-pitfall`),
//! not a porting error: the port follows Go (govec). Known differences are in `KNOWN`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;

use serde_json::Value;
use tidb_kv::key::{Handle, IntHandle};
use tidb_query_datatype::codec::data_type::ScalarValue;
use tidb_query_datatype::codec::mysql::{Decimal as KDecimal, Duration as KDuration, Json as KJson, Time as KTime, TimeType};
use tidb_query_datatype::codec::row::v2::encoder_for_test::{Column as KColumn, RowEncoder};
use tidb_query_datatype::codec::row::v2::{RowSlice, V1CompatibleEncoder};
use tidb_query_datatype::codec::table as kt;
use tidb_query_datatype::expr::EvalContext;
use tidb_query_datatype::prelude::FieldTypeAccessor;
use tidb_query_datatype::{FieldTypeFlag, FieldTypeTp};
use tidb_tablecodec as tc;
use tidb_tablecodec::rowcodec::{self, row::Row};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") { "amd64" } else { "arm64" };

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn records() -> Vec<Value> {
    let path = format!("{}/tests/vectors/v8.5.8/{GO_TARGET}/tablecodec.jsonl.gz", env!("CARGO_MANIFEST_DIR"));
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    text.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn ints() -> Vec<i64> {
    let mut v = vec![0, 1, -1, 255, 256, i64::MAX, i64::MIN, 1 << 32, -(1 << 32)];
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for _ in 0..300 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push((x as i64) >> (x % 64));
    }
    v
}

#[test]
fn keys_both_ways() {
    let ints = ints();
    for (i, &tid) in ints.iter().enumerate() {
        let h = ints[(i * 7 + 3) % ints.len()];
        let ours = tc::encode_row_key_with_handle(tid, &Handle::Int(IntHandle(h))).0;
        assert_eq!(ours, kt::encode_row_key(tid, h), "encode_row_key({tid}, {h})");
        let enc = format!("{h:x}").into_bytes();
        assert_eq!(tc::encode_row_key(tid, &enc).0, kt::encode_common_handle(tid, &enc));
        assert_eq!(tc::encode_index_seek_key(tid, h, &enc).0, kt::encode_index_seek_key(tid, h, &enc));
        // TiKV-written keys decode in tidb-rs, and tidb-rs keys in TiKV.
        let tk = kt::encode_row_key(tid, h);
        assert_eq!(tc::decode_table_id(&tk), tid);
        match tc::decode_row_key(&tk).unwrap() {
            Handle::Int(x) => assert_eq!(x.0, h),
            other => panic!("{other:?}"),
        }
        assert_eq!(kt::decode_table_id(&ours).unwrap(), tid);
        assert_eq!(kt::decode_int_handle(&ours).unwrap(), h);
        assert_eq!(kt::check_record_key(&ours).is_ok(), tc::is_record_key(&ours));
        let ik = tc::encode_index_seek_key(tid, h, &[3, 1, 2]).0;
        assert_eq!(kt::check_index_key(&ik).is_ok(), tc::is_index_key(&ik));
        let ck = tc::encode_row_key(tid, &enc).0;
        assert_eq!(kt::decode_common_handle(&ck).unwrap(), &enc[..]);
    }
}

fn tikv_ft(ft: &tidb_parser::types::FieldType) -> tipb::FieldType {
    let mut k = tipb::FieldType::default();
    k.as_mut_accessor().set_tp(FieldTypeTp::from_i32(ft.get_type() as i32).unwrap());
    k.as_mut_accessor().set_flag(FieldTypeFlag::from_bits_truncate(ft.get_flag() as u32));
    k.as_mut_accessor().set_decimal(ft.get_decimal() as isize);
    k
}

/// The TiKV value of a v2 column (`ScalarValue`), from the column bytes and its Go type.
fn tikv_value(ctx: &mut EvalContext, data: &[u8], ft: &tidb_parser::types::FieldType) -> Option<ScalarValue> {
    use tidb_base::mysql as m;
    // The generator gives some columns another type than their data (decoder robustness):
    // re-encoding is checked only where the bytes fit the type.
    let int_width = matches!(data.len(), 1 | 2 | 4 | 8);
    let u = || rowcodec_uint(data);
    Some(match ft.get_type() {
        m::TypeTiny | m::TypeShort | m::TypeInt24 | m::TypeLong | m::TypeLonglong | m::TypeYear | m::TypeEnum
        | m::TypeSet | m::TypeBit | m::TypeDate | m::TypeDatetime | m::TypeTimestamp | m::TypeDuration
            if !int_width =>
        {
            return None;
        }
        m::TypeFloat | m::TypeDouble if data.len() != 8 => return None,
        m::TypeTiny | m::TypeShort | m::TypeInt24 | m::TypeLong | m::TypeLonglong | m::TypeYear => {
            // Go writes ints by the datum's kind (int64 or uint64), not by the column flag;
            // the generator's rows may mix them: the signedness whose minimal width is the
            // stored width is the one Go used.
            let i = rowcodec_int(data);
            let signed_width = int_width_of(i);
            if signed_width == data.len() && !m::has_unsigned_flag(ft.get_flag()) {
                ScalarValue::Int(Some(i))
            } else if m::has_unsigned_flag(ft.get_flag()) && uint_width(u()) == data.len() {
                ScalarValue::Int(Some(u() as i64))
            } else {
                return None;
            }
        }
        m::TypeEnum | m::TypeSet | m::TypeBit if uint_width(u()) == data.len() => ScalarValue::Int(Some(u() as i64)),
        m::TypeEnum | m::TypeSet | m::TypeBit => return None,
        m::TypeFloat | m::TypeDouble => {
            let (_, f) = tidb_codec::decode_float(data).ok()?;
            ScalarValue::Real(Some(tidb_query_datatype::codec::data_type::Real::new(f).ok()?))
        }
        m::TypeVarchar | m::TypeVarString | m::TypeString | m::TypeBlob | m::TypeTinyBlob | m::TypeMediumBlob
        | m::TypeLongBlob => ScalarValue::Bytes(Some(data.to_vec())),
        m::TypeNewDecimal => {
            let (rest, dec, _, _, err) = tidb_codec::decode_decimal(data);
            err.ok()?;
            if !rest.is_empty() {
                return None;
            }
            let s = String::from_utf8(dec?.to_string()).ok()?;
            ScalarValue::Decimal(Some(s.parse::<KDecimal>().ok()?))
        }
        // A packed time with microseconds above 999999 is not a time (a swapped type).
        m::TypeDate | m::TypeDatetime | m::TypeTimestamp if u() & 0xff_ffff > 999_999 || uint_width(u()) != data.len() => {
            return None;
        }
        m::TypeDuration if int_width_of(rowcodec_int(data)) != data.len() => return None,
        m::TypeDate | m::TypeDatetime | m::TypeTimestamp => {
            let tt = match ft.get_type() {
                m::TypeDate => TimeType::Date,
                m::TypeDatetime => TimeType::DateTime,
                _ => TimeType::Timestamp,
            };
            ScalarValue::DateTime(Some(KTime::from_packed_u64(ctx, u(), tt, ft.get_decimal() as i8).ok()?))
        }
        // TiKV rounds a duration to its fsp on construction; the generator's durations are not
        // rounded to the column fsp (TiDB's are): fsp 6 keeps the stored nanoseconds.
        m::TypeDuration => ScalarValue::Duration(Some(KDuration::from_nanos(rowcodec_int(data), 6).ok()?)),
        m::TypeJSON => {
            let tp = tidb_query_datatype::codec::mysql::json::JsonType::try_from(data[0]).ok()?;
            ScalarValue::Json(Some(KJson::new(tp, data[1..].to_vec())))
        }
        _ => return None,
    })
}

fn int_width_of(i: i64) -> usize {
    if i == i as i8 as i64 {
        1
    } else if i == i as i16 as i64 {
        2
    } else if i == i as i32 as i64 {
        4
    } else {
        8
    }
}

fn uint_width(u: u64) -> usize {
    if u <= 0xff {
        1
    } else if u <= 0xffff {
        2
    } else if u <= 0xffff_ffff {
        4
    } else {
        8
    }
}

fn rowcodec_int(v: &[u8]) -> i64 {
    match v.len() {
        1 => v[0] as i8 as i64,
        2 => i16::from_le_bytes([v[0], v[1]]) as i64,
        4 => i32::from_le_bytes([v[0], v[1], v[2], v[3]]) as i64,
        _ => i64::from_le_bytes(v[..8].try_into().unwrap()),
    }
}

fn rowcodec_uint(v: &[u8]) -> u64 {
    match v.len() {
        1 => v[0] as u64,
        2 => u16::from_le_bytes([v[0], v[1]]) as u64,
        4 => u32::from_le_bytes([v[0], v[1], v[2], v[3]]) as u64,
        _ => u64::from_le_bytes(v[..8].try_into().unwrap()),
    }
}

struct Stats {
    rows: usize,
    reencoded: usize,
    compat_cols: usize,
    compat_bytes_differ: usize,
    v1_rows: usize,
    known_checksum_len: usize,
}

/// A v1 datum's value, kind-normalised (ints and durations as i64).
fn datum_value(b: &[u8]) -> String {
    use tidb_types::datum as dm;
    let (_, d, err) = tidb_codec::decode_one(b);
    if err.is_err() {
        return format!("err {}", hex(b));
    }
    match d.kind() {
        dm::KindNull => "null".into(),
        dm::KindInt64 => format!("i{}", d.get_int64()),
        dm::KindMysqlDuration => format!("i{}", d.get_mysql_duration().duration),
        dm::KindUint64 => format!("u{}", d.get_uint64()),
        dm::KindFloat64 | dm::KindFloat32 => format!("f{:x}", d.get_float64().to_bits()),
        dm::KindMysqlDecimal => format!("d{}", String::from_utf8_lossy(&d.get_mysql_decimal().to_string())),
        k => format!("k{k} {}", hex(d.get_bytes())),
    }
}

#[test]
fn rows_both_ways() {
    let mut st = Stats { rows: 0, reencoded: 0, compat_cols: 0, compat_bytes_differ: 0, v1_rows: 0, known_checksum_len: 0 };
    let mut ctx = EvalContext::default();
    for rec in records() {
        // Mutated rows (unsorted column IDs, trailing bytes, corrupted counts) are malformed:
        // the two binary searches and length checks legitimately differ on them.
        if rec["op"] != "row" || rec["src"] == "rand/row_mut" {
            continue;
        }
        let a = rec["args"].as_array().unwrap();
        let out = &rec["out"];
        let b = unhex(a[0].as_str().unwrap());
        let infos: Vec<(i64, tidb_parser::types::FieldType)> = a[5]
            .as_array()
            .map(|x| x.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(|q| (q[0].as_i64().unwrap(), ft_in(&q[3])))
            .collect();
        if b.first() == Some(&rowcodec::CodecVer) {
            // Rows Go decodes without error (datum-map decoder).
            if out["dm"].get("panic").is_some() || !out["dm"]["err"].is_null() {
                continue;
            }
            st.rows += 1;
            let mut ours = Row::default();
            ours.from_bytes(&b).unwrap();
            let b = b.clone();
            // KNOWN (upstream-pitfall candidate): TiKV asserts that the checksum section is
            // exactly 5 or 9 bytes and panics otherwise; Go reads the header and checksums and
            // ignores trailing bytes. TiDB writes 5 or 9 bytes, so only a malformed value
            // (here: a second checksum appended) reaches it.
            let ks = match std::panic::catch_unwind(|| RowSlice::from_bytes(&b)) {
                Ok(ks) => ks.unwrap(),
                Err(_) => {
                    assert!(ours.get_checksum().1, "TiKV panicked on a row without checksum: {}", hex(&b));
                    st.known_checksum_len += 1;
                    continue;
                }
            };
            let (c1, ok1) = ours.get_checksum();
            match ks.get_checksum() {
                Some(c) => {
                    assert!(ok1);
                    assert_eq!(c.get_checksum_val(), c1, "checksum of {}", hex(&b));
                }
                None => assert!(!ok1),
            }
            let mut cols = vec![];
            let mut all_supported = true;
            let mut ids_seen = HashSet::new();
            for (id, ft) in &infos {
                if !ids_seen.insert(*id) {
                    continue;
                }
                let ours_data = rowcodec_col(&ours, *id);
                let kd = ks.search_in_non_null_ids(*id).unwrap().map(|(s, e)| &ks.values()[s..e]);
                assert_eq!(ours_data.0, kd, "column {id} of {}", hex(&b));
                assert_eq!(ours_data.1, ks.search_in_null_ids(*id), "null {id} of {}", hex(&b));
                if let Some(data) = kd {
                    // v2 -> v1 datum: TiKV's conversion and Go's encodeOldDatum.
                    let mut tk = vec![];
                    if tk.write_v2_as_datum(data, &tikv_ft(ft)).is_ok() {
                        let dec = rowcodec::new_byte_decoder(
                            vec![rowcodec::ColInfo { id: *id, ft: Arc::new(ft.clone()), ..Default::default() }],
                            vec![],
                            None,
                            None,
                        );
                        let mut offs = HashMap::new();
                        offs.insert(*id, 0usize);
                        let r = dec.decode_to_bytes_no_handle(&offs, &b).unwrap();
                        // The v1 datum bytes differ by design (Go: varint / varuint flags,
                        // durations as ints; TiKV: comparable ints, a duration flag): TiKV
                        // consumes its own conversion only. The values must be equal.
                        assert_eq!(
                            datum_value(&r[0]),
                            datum_value(&tk),
                            "v1 datum value of column {id} (type {}) of {}: go {} tikv {}",
                            ft.get_type(),
                            hex(&b),
                            hex(&r[0]),
                            hex(&tk)
                        );
                        if r[0] != tk {
                            st.compat_bytes_differ += 1;
                        }
                        st.compat_cols += 1;
                    }
                    match tikv_value(&mut ctx, data, ft) {
                        Some(v) => cols.push(KColumn::new_with_ft(*id, tikv_ft(ft), v)),
                        None => all_supported = false,
                    }
                } else if ks.search_in_null_ids(*id) {
                    cols.push(KColumn::new_with_ft(*id, tikv_ft(ft), ScalarValue::Int(None)));
                }
            }
            // TiKV re-encodes the same values to Go's bytes (rows without checksum whose every
            // column is in the infos and of a type TiKV's test encoder writes).
            let ncols = ours.num_cols();
            if all_supported && ks.get_checksum().is_none() && cols.len() == ncols {
                let mut buf = vec![];
                buf.write_row(&mut ctx, cols).unwrap();
                assert_eq!(hex(&buf), hex(&b), "TiKV re-encoding");
                st.reencoded += 1;
            }
        } else if !b.is_empty() && out["v1"].get("panic").is_none() && out["v1"]["err"].is_null() {
            // v1 rows: TiKV cut_row and CutRowNew.
            let ids: collections::HashSet<i64> = infos.iter().map(|x| x.0).collect();
            let mut kcols = vec![];
            for (id, ft) in &infos {
                let mut ci = tipb::ColumnInfo::default();
                ci.set_column_id(*id);
                ci.as_mut_accessor().set_tp(FieldTypeTp::from_i32(ft.get_type() as i32).unwrap());
                kcols.push(ci);
            }
            let Ok(dict) = kt::cut_row(b.clone(), &ids, kcols.into()) else { continue };
            let mut offs = HashMap::new();
            for (id, _) in &infos {
                let n = offs.len();
                offs.entry(*id).or_insert(n);
            }
            let Ok(cut) = tc::cut_row_new(&b, &offs) else { continue };
            if cut.len() != offs.len() {
                continue; // the one-byte NilFlag row: Go returns nil
            }
            for (id, i) in &offs {
                let k = dict.get(*id).unwrap_or(&[]);
                assert_eq!(hex(cut[*i]), hex(k), "v1 column {id} of {}", hex(&b));
            }
            st.v1_rows += 1;
        }
    }
    eprintln!(
        "TiKV oracle: {} v2 rows parsed, {} re-encoded, {} v1 datum conversions ({} with other bytes), {} v1 rows cut, {} rows TiKV rejects (checksum section length)",
        st.rows, st.reencoded, st.compat_cols, st.compat_bytes_differ, st.v1_rows, st.known_checksum_len
    );
    assert!(st.rows > 500 && st.reencoded > 100 && st.compat_cols > 1000 && st.v1_rows > 100);
}

/// (data, isNull) of a column in our row view (via the BytesDecoder-free accessors).
fn rowcodec_col<'a>(r: &Row<'a>, id: i64) -> (Option<&'a [u8]>, bool) {
    let (idx, is_nil, not_found) = r.find_col_id(id);
    if !not_found && !is_nil {
        (Some(r.get_data(idx)), false)
    } else {
        (None, is_nil)
    }
}

fn ft_in(v: &Value) -> tidb_parser::types::FieldType {
    let p = v.as_array().unwrap();
    let mut ft = tidb_types::field_type_builder::new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p();
    if !p[6].is_null() {
        ft.set_elems(Some(p[6].as_array().unwrap().iter().map(|x| x.as_str().unwrap_or("").as_bytes().to_vec()).collect()));
    }
    ft
}
