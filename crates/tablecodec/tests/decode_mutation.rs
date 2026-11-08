//! Decode robustness: corrupted rows, index keys / values and keys (truncation, byte flips,
//! extreme counts and lengths) through every decoder of the crate must end in an error or a
//! catchable panic — never an abort, a hang or a huge allocation. Go panics on many of these
//! inputs too (govec records them as `{"panic": true}`); this test only checks that the Rust
//! port fails cleanly.
//!
//! Index values with restored data are not mutated: a corrupted restored `_bin` padding count
//! asks for up to 2^63 spaces (spec §5); the port panics on the failed allocation, but a
//! count of a few GiB would really be allocated, as in Go.

use std::collections::HashMap;
use std::io::Read;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use serde_json::Value;
use tidb_tablecodec as tc;
use tidb_tablecodec::rowcodec::{self, ColInfo};

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") { "amd64" } else { "arm64" };

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn n(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Rng, b: &[u8]) -> Vec<u8> {
    let mut c = b.to_vec();
    for _ in 0..1 + rng.n(3) {
        match rng.n(6) {
            0 if !c.is_empty() => c.truncate(rng.n(c.len())),
            1 if !c.is_empty() => {
                let i = rng.n(c.len());
                c[i] ^= 1 + rng.n(255) as u8;
            }
            2 => c.push(rng.n(256) as u8),
            3 if !c.is_empty() => {
                let i = rng.n(c.len());
                c[i] = [0, 0x7f, 0x80, 0xff, 1, 3, 125, 126, 127, 128][rng.n(10)];
            }
            4 if c.len() > 6 => {
                // extreme column counts / lengths
                let i = 2 + 2 * rng.n(2);
                c[i] = 0xff;
                c[i + 1] = 0xff;
            }
            _ => {}
        }
    }
    c
}

fn ft_in(v: &Value) -> tidb_parser::types::FieldType {
    let p = v.as_array().unwrap();
    tidb_types::field_type_builder::new_field_type_builder()
        .set_type(p[0].as_u64().unwrap() as u8)
        .set_flag(p[1].as_u64().unwrap() as usize)
        .set_flen(p[2].as_i64().unwrap() as isize)
        .set_decimal(p[3].as_i64().unwrap() as isize)
        .set_charset(p[4].as_str().unwrap())
        .set_collate(p[5].as_str().unwrap())
        .build_p()
}

fn col_infos(v: &Value) -> Vec<ColInfo> {
    v.as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|q| ColInfo { id: q[0].as_i64().unwrap(), ft: Arc::new(ft_in(&q[3])), ..ColInfo::default() })
        .collect()
}

#[test]
fn decode_mutation() {
    let path = format!("{}/tests/vectors/v8.5.8/{GO_TARGET}/tablecodec.jsonl.gz", env!("CARGO_MANIFEST_DIR"));
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let recs: Vec<Value> = text.lines().skip(1).map(|l| serde_json::from_str(l).unwrap()).collect();
    let rows: Vec<&Value> = recs.iter().filter(|r| r["op"] == "row").collect();
    let idx: Vec<&Value> = recs.iter().filter(|r| r["op"] == "index_raw").collect();
    let keys: Vec<&Value> = recs.iter().filter(|r| r["op"] == "key_decode").collect();
    let loc = tidb_base::gostd::time::fixed_zone("Z", 28800);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut rng = Rng(0x2545F4914F6CDD1D);
    let (mut ok, mut err, mut panics) = (0usize, 0usize, 0usize);
    let mut tally = |r: std::thread::Result<bool>| match r {
        Ok(true) => ok += 1,
        Ok(false) => err += 1,
        Err(_) => panics += 1,
    };
    for _ in 0..20_000 {
        let r = rows[rng.n(rows.len())];
        let a = r["args"].as_array().unwrap();
        let b = mutate(&mut rng, &unhex(a[0].as_str().unwrap()));
        let infos = col_infos(&a[5]);
        let mut cols = HashMap::new();
        for c in &infos {
            cols.insert(c.id, Arc::clone(&c.ft));
        }
        let mut offs = HashMap::new();
        for c in &infos {
            let n = offs.len();
            offs.entry(c.id).or_insert(n);
        }
        let l = Some(&loc);
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_row_to_datum_map(&b, &cols, l).is_ok())));
        tally(catch_unwind(AssertUnwindSafe(|| tc::cut_row_new(&b, &offs).is_ok())));
        let fts: Vec<_> = infos.iter().map(|c| (*c.ft).clone()).collect();
        tally(catch_unwind(AssertUnwindSafe(|| {
            let mut chk = tidb_chunk::chunk::new_chunk_with_capacity(&fts, 1);
            rowcodec::new_chunk_decoder(infos.clone(), vec![], None, Some(loc.clone()))
                .decode_to_chunk(&b, 0, None, &mut chk)
                .is_ok()
        })));
        tally(catch_unwind(AssertUnwindSafe(|| {
            rowcodec::new_byte_decoder(infos.clone(), vec![], None, None).decode_to_bytes_no_handle(&offs, &b).is_ok()
        })));

        let x = idx[rng.n(idx.len())];
        let a = x["args"].as_array().unwrap();
        let mut k = unhex(a[0].as_str().unwrap());
        let mut v = unhex(a[1].as_str().unwrap());
        let restored = catch_unwind(|| tc::split_index_value(&v).restored_values.is_some()).unwrap_or(true);
        if rng.n(2) == 0 && !restored {
            v = mutate(&mut rng, &v);
        } else {
            k = mutate(&mut rng, &k);
        }
        let n = a[2].as_u64().unwrap() as usize;
        let ci = col_infos(&a[3]);
        tally(catch_unwind(AssertUnwindSafe(|| {
            tc::decode_index_kv(&k, &v, n, tc::HandleStatus::HandleDefault, &ci).is_ok()
        })));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_index_handle(&k, &v, n).is_ok())));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_handle_in_index_value(&v).is_ok())));

        let ki = rng.n(keys.len());
        let key = mutate(&mut rng, &unhex(keys[ki]["args"][0].as_str().unwrap()));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_record_key(&key).is_ok())));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_index_key(&key).is_ok())));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_meta_key(&key).is_ok())));
        tally(catch_unwind(AssertUnwindSafe(|| tc::decode_table_id(&key) != 0)));
    }
    std::panic::set_hook(prev);
    eprintln!("decode_mutation: {ok} ok, {err} errors, {panics} catchable panics");
    assert!(ok + err + panics == 20_000 * 11);
}
