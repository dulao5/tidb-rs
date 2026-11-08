//! D01.8 codec benchmarks, the same workloads as the Go side in tidb-rs-tracker
//! `govec/domains/codec_bench_test.go`: the five of `pkg/util/codec/bench_test.go`
//! (DecodeOneToChunk resets its chunk every 1024 rows) and the point-get / index
//! (EncodeKey, DecodeOne) and hash join / aggregation (HashChunkRow, HashChunkSelected,
//! SerializeKeys) paths.
//!
//! `cargo bench -p tidb-codec`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use tidb_base::gostd::time::utc;
use tidb_base::mysql;
use tidb_chunk::Chunk;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_codec as tc;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::time::new_time;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Go's hash/fnv New64 (FNV-1, 64 bit).
struct Fnv64(u64);
impl Default for Fnv64 {
    fn default() -> Self {
        Fnv64(14695981039346656037)
    }
}
impl std::hash::Hasher for Fnv64 {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self.0.wrapping_mul(1099511628211) ^ u64::from(b);
        }
    }
}

fn ints100() -> Vec<u8> {
    let values: Vec<Datum> = (0..100).map(dm::new_int_datum).collect();
    let mut bs = Vec::new();
    tc::encode_value(&utc(), &mut bs, &values).unwrap();
    bs
}

fn tuple() -> Vec<Datum> {
    let mut dec = MyDecimal::default();
    dec.from_string(b"12345.678").unwrap();
    // 2011-11-10 11:11:11.123456 as a DATETIME(6).
    let mut t = new_time(tidb_types::core_time::CoreTime(0), mysql::TypeDatetime, 6);
    t.from_packed_uint(1_840_814_726_643_507_776).unwrap();
    let mut s = Datum::default();
    s.set_string(
        "Hello, World — ÄÖÜ".as_bytes().to_vec(),
        "utf8mb4_general_ci",
    );
    vec![
        dm::new_int_datum(-123456789),
        s,
        dm::new_decimal_datum(dec),
        dm::new_time_datum(t),
        dm::new_float64_datum(3.25),
    ]
}

fn bench_chunk() -> (Vec<FieldType>, Chunk) {
    collate::set_new_collation_enabled_for_test(true);
    let fts = vec![
        FieldType::new(mysql::TypeLonglong),
        new_field_type_builder()
            .set_type(mysql::TypeVarchar)
            .set_flen(64)
            .set_charset("utf8mb4")
            .set_collate("utf8mb4_general_ci")
            .build_p(),
        new_field_type_builder()
            .set_type(mysql::TypeNewDecimal)
            .set_flen(10)
            .set_decimal(3)
            .build_p(),
    ];
    let mut chk = new_chunk_with_capacity(&fts, 1024);
    let strs = ["apple", "Banana", "cherry pie", "Ärger", "straße"];
    for i in 0..1024i64 {
        chk.append_int64(0, i * 7919 - 3_000_000);
        chk.append_string(1, strs[i as usize % 5].as_bytes());
        chk.append_my_decimal(2, &tidb_types::new_dec_from_int(i * 37));
    }
    (fts, chk)
}

fn benches(c: &mut Criterion) {
    let loc = utc();
    let bs = ints100();
    c.bench_function("DecodeWithSize", |b| {
        b.iter(|| tc::decode(black_box(&bs), 100).unwrap())
    });
    c.bench_function("DecodeWithOutSize", |b| {
        b.iter(|| tc::decode(black_box(&bs), 1).unwrap())
    });
    c.bench_function("EncodeIntWithSize", |b| {
        b.iter(|| {
            let mut data = Vec::with_capacity(8);
            tc::encode_int(&mut data, black_box(10));
            data
        })
    });
    let mut dec = MyDecimal::default();
    dec.from_float64(1211.1211113).unwrap();
    let (p, f) = dec.precision_and_frac();
    let mut raw = Vec::new();
    tc::encode_decimal(&mut raw, &dec, p, f).unwrap();
    c.bench_function("DecodeDecimal", |b| {
        b.iter(|| tc::decode_decimal(black_box(&raw)).4.unwrap())
    });

    let mut raw = vec![1u8];
    tc::encode_bytes(&mut raw, b"a");
    let ft = FieldType::new(mysql::TypeVarchar);
    let mut chk = new_chunk_with_capacity(std::slice::from_ref(&ft), 1024);
    c.bench_function("DecodeOneToChunk", |b| {
        let mut d = tc::new_decoder(&mut chk, loc.clone());
        let mut i = 0usize;
        b.iter(|| {
            if i % 1024 == 0 {
                d.chunk().reset();
            }
            i += 1;
            d.decode_one(black_box(&raw), 0, &ft).unwrap().len()
        })
    });

    collate::set_new_collation_enabled_for_test(true);
    let ds = tuple();
    let mut buf = Vec::with_capacity(128);
    c.bench_function("EncodeKey", |b| {
        b.iter(|| {
            buf.clear();
            tc::encode_key(&loc, &mut buf, black_box(&ds)).unwrap();
        })
    });
    let mut vbs = Vec::new();
    tc::encode_value(&loc, &mut vbs, &ds).unwrap();
    c.bench_function("DecodeOne", |b| {
        b.iter(|| {
            let mut rest: &[u8] = black_box(&vbs);
            while !rest.is_empty() {
                let (r, d, err) = tc::decode_one(rest);
                err.unwrap();
                black_box(d);
                rest = r;
            }
        })
    });

    let (fts, chk) = bench_chunk();
    let ctx = &*DefaultStmtNoWarningContext;
    let col_idx = [0usize, 1, 2];
    c.bench_function("HashChunkRow", |b| {
        let mut w: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1];
        b.iter(|| {
            for i in 0..1024 {
                w.clear();
                tc::hash_chunk_row(ctx, &mut w, chk.get_row(i), &fts, &col_idx, &mut buf).unwrap();
            }
        })
    });
    c.bench_function("HashChunkSelected", |b| {
        let mut is_null = vec![false; 1024];
        let mut buf = [0u8; 1];
        b.iter(|| {
            let mut hs: Vec<Fnv64> = (0..1024).map(|_| Fnv64::default()).collect();
            for (col, ft) in fts.iter().enumerate() {
                tc::hash_chunk_selected(
                    ctx,
                    &mut hs,
                    &chk,
                    ft,
                    col,
                    &mut buf,
                    &mut is_null,
                    None,
                    false,
                )
                .unwrap();
            }
            hs
        })
    });
    let used: Vec<usize> = (0..1024).collect();
    c.bench_function("SerializeKeys", |b| {
        let mut keys: Vec<Vec<u8>> = vec![Vec::new(); 1024];
        let mut nulls = vec![false; 1024];
        b.iter(|| {
            for k in keys.iter_mut() {
                k.clear();
            }
            for (col, ft) in fts.iter().enumerate() {
                tc::serialize_keys(
                    ctx,
                    &chk,
                    ft,
                    col,
                    &used,
                    None,
                    Some(&mut nulls),
                    tc::KeepVarColumnLength,
                    &mut keys,
                )
                .unwrap();
            }
        })
    });
}

criterion_group!(codec_benches, benches);
criterion_main!(codec_benches);
