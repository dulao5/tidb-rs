//! Decoding malformed codec bytes (keys and values read from storage, coprocessor results):
//! truncations, flipped flag bytes, random byte flips and length / marker bytes pushed to
//! their limits must end in an error or a catchable panic, never an abort, a hang or a huge
//! allocation. Go's decoders also panic on some malformed input (for example
//! `DecodeDecimal`'s `b[binSize:]` past the end), and govec records require the same panics;
//! this test is the robustness side: every one of the decoders over many mutations.

use std::alloc::{GlobalAlloc, Layout, System};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tidb_base::gostd::time::{fixed_zone, utc};
use tidb_base::mysql;
use tidb_chunk::chunk::new_chunk_with_capacity;
use tidb_codec as tc;
use tidb_parser::types::FieldType;
use tidb_types::MyDecimal;
use tidb_types::binary_literal::BinaryLiteral;
use tidb_types::core_time::CoreTime;
use tidb_types::datum::{self as dm, Datum};
use tidb_types::json::parse_binary_json_from_string;
use tidb_types::time::{Duration, new_time};
use tidb_types::vector::parse_vector_float32;

/// The largest single allocation the test binary makes.
struct Counting;
static MAX_ALLOC: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        MAX_ALLOC.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        MAX_ALLOC.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// xorshift64: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn datums() -> Vec<Datum> {
    let mut v = vec![
        Datum::default(),
        dm::new_int_datum(-5),
        dm::new_int_datum(i64::MAX),
        dm::new_uint_datum(u64::MAX),
        dm::new_float64_datum(-1.5),
        dm::new_bytes_datum(b"hello world, a longer string".to_vec()),
        dm::new_bytes_datum(Vec::new()),
        dm::new_duration_datum(Duration {
            duration: -3_723_000_000_000,
            fsp: 3,
        }),
        dm::new_binary_literal_datum(BinaryLiteral(vec![1, 2, 3])),
    ];
    for s in [
        "0",
        "-123.456",
        "99999999999999999999.999999999",
        "0.000001",
    ] {
        let mut d = MyDecimal::default();
        d.from_string(s.as_bytes()).unwrap();
        v.push(dm::new_decimal_datum(d));
    }
    let mut t = new_time(CoreTime(0), mysql::TypeDatetime, 6);
    t.from_packed_uint(1_840_814_726_643_384_320).unwrap();
    v.push(dm::new_time_datum(t));
    for js in ["[1,\"ab\",{\"k\":null}]", "1.5", "\"s\""] {
        v.push(dm::new_json_datum(
            parse_binary_json_from_string(js.as_bytes()).unwrap(),
        ));
    }
    v.push(dm::new_vector_float32_datum(
        parse_vector_float32(b"[1,2.5,-3]").unwrap(),
    ));
    v
}

fn fts() -> Vec<FieldType> {
    let mut v: Vec<FieldType> = [
        mysql::TypeLonglong,
        mysql::TypeDouble,
        mysql::TypeFloat,
        mysql::TypeVarchar,
        mysql::TypeNewDecimal,
        mysql::TypeDuration,
        mysql::TypeDatetime,
        mysql::TypeTimestamp,
        mysql::TypeJSON,
        mysql::TypeTiDBVectorFloat32,
    ]
    .into_iter()
    .map(FieldType::new)
    .collect();
    let mut e = FieldType::new(mysql::TypeEnum);
    e.set_elems(Some(vec![b"a".to_vec(), b"b".to_vec()]));
    v.push(e);
    let mut s = FieldType::new(mysql::TypeSet);
    s.set_elems(Some(vec![b"a".to_vec(), b"b".to_vec()]));
    v.push(s);
    let mut b = FieldType::new(mysql::TypeBit);
    b.set_flen(10);
    v.push(b);
    v
}

fn mutate(rng: &mut Rng, b: &[u8]) -> Vec<u8> {
    let mut c = b.to_vec();
    if c.is_empty() {
        return vec![rng.next() as u8];
    }
    match rng.below(5) {
        0 => c.truncate(rng.below(c.len())),
        1 => c[0] = rng.next() as u8,
        2 => {
            let i = rng.below(c.len());
            c[i] ^= 1 + rng.below(255) as u8;
        }
        3 => {
            // Lengths, varints and markers pushed to their limits.
            let fill = [0x00u8, 0x7f, 0x80, 0xff, 0xfe, 0x01][rng.below(6)];
            let from = 1 + rng.below(c.len());
            for x in c.iter_mut().skip(from).take(1 + rng.below(10)) {
                *x = fill;
            }
        }
        _ => {
            let n = rng.below(16);
            for _ in 0..n {
                c.push(rng.next() as u8);
            }
        }
    }
    c
}

/// Every decoder over `b`. Errors are fine, panics are caught by the caller.
fn decode_everything(b: &[u8], fts: &[FieldType]) {
    let (rem, d, err) = tc::decode_one(b);
    std::hint::black_box((rem, d.kind(), err.is_ok()));
    std::hint::black_box(tc::decode(b, 4).map(|v| v.len()).ok());
    std::hint::black_box(tc::cut_one(b).map(|(a, r)| a.len() + r.len()).ok());
    std::hint::black_box(tc::cut_column_id(b).ok().map(|x| x.1));
    let mut raws = vec![Datum::default(); 3];
    std::hint::black_box(tc::set_raw_values(b, &mut raws).is_ok());
    let (vals, rem, _) = tc::decode_range(b, 3, None, None);
    std::hint::black_box((vals.len(), rem.len()));
    let tps = [mysql::TypeDatetime, mysql::TypeFloat, mysql::TypeTimestamp];
    let loc = fixed_zone("Z", 3600);
    let (vals, rem, _) = tc::decode_range(b, 3, Some(&tps), Some(&loc));
    std::hint::black_box((vals.len(), rem.len()));
    for prim in [
        tc::decode_int(b).map(|x| x.1 as u64).ok(),
        tc::decode_uvarint(b).map(|x| x.1).ok(),
        tc::decode_comparable_varint(b).map(|x| x.1 as u64).ok(),
        tc::decode_comparable_uvarint(b).map(|x| x.1).ok(),
        tc::decode_bytes(b, None).map(|x| x.1.len() as u64).ok(),
        tc::decode_bytes_desc(b, None)
            .map(|x| x.1.len() as u64)
            .ok(),
        tc::decode_compact_bytes(b).map(|x| x.1.len() as u64).ok(),
    ] {
        std::hint::black_box(prim);
    }
    std::hint::black_box(tc::decode_decimal(b).4.is_ok());
    let mut chk = new_chunk_with_capacity(fts, 2);
    {
        let mut dec = tc::new_decoder(&mut chk, utc());
        for (c, ft) in fts.iter().enumerate() {
            std::hint::black_box(dec.decode_one(b, c, ft).is_ok());
        }
    }
}

#[test]
fn malformed_input_never_aborts() {
    let fts = fts();
    let loc = utc();
    let mut bases = vec![];
    for d in datums() {
        for key in [true, false] {
            let mut b = Vec::new();
            let r = if key {
                tc::encode_key(&loc, &mut b, std::slice::from_ref(&d))
            } else {
                tc::encode_value(&loc, &mut b, std::slice::from_ref(&d))
            };
            r.unwrap();
            bases.push(b);
        }
    }
    // Tuples of several datums.
    let all = datums();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    for _ in 0..50 {
        let n = 1 + rng.below(4);
        let ds: Vec<Datum> = (0..n).map(|_| all[rng.below(all.len())].clone()).collect();
        let mut b = Vec::new();
        tc::encode_value(&loc, &mut b, &ds).unwrap();
        bases.push(b);
    }

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let start = Instant::now();
    let (mut ok, mut panics) = (0, 0);
    for _ in 0..20_000 {
        let base = &bases[rng.below(bases.len())];
        let m = mutate(&mut rng, base);
        let m = if rng.below(3) == 0 {
            mutate(&mut rng, &m)
        } else {
            m
        };
        match catch_unwind(AssertUnwindSafe(|| decode_everything(&m, &fts))) {
            Ok(()) => ok += 1,
            Err(_) => panics += 1,
        }
    }
    std::panic::set_hook(prev);
    let elapsed = start.elapsed();
    assert_eq!(ok + panics, 20_000);
    assert!(
        elapsed.as_secs() < 120,
        "20,000 malformed inputs took {elapsed:?} (a hang?)"
    );
    let max = MAX_ALLOC.load(Ordering::Relaxed);
    assert!(
        max < 16 << 20,
        "a single allocation of {max} bytes while decoding malformed input"
    );
    eprintln!(
        "malformed inputs: {ok} decoded or errored, {panics} caught panics, max alloc {max} B, {elapsed:?}"
    );
}
