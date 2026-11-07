//! Port of Go's pkg/types/mydecimal_benchmark_test.go (same inputs, same loop bodies).
//!
//! Go: `go test -bench 'Round|ToFloat64|ToBin|ToHashKey' -run '^$' ./pkg/types/`
//! Rust: `cargo bench -p tidb-types --bench mydecimal`
//!
//! The global allocator is jemalloc, as in the server (A1 / P0 decision D7); Go's numbers
//! are with Go's runtime allocator. Allocation-heavy benchmarks (ToBin, ToHashKey) depend
//! on it.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tidb_types::{
    MODE_CEILING, MODE_HALF_UP, MODE_TRUNCATE, MyDecimal, gostd, new_dec_from_float_for_test,
};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const NUM_TEST_DEC: usize = 1000;

/// Go's math/rand (unseeded) replaced by a fixed-seed xorshift; the shapes of the
/// generated values are the same as Go's genTestDecimals.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// rand.Float64: [0, 1)
    fn float64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// rand.Int: non-negative int
    fn int(&mut self) -> i64 {
        (self.next_u64() >> 1) as i64
    }
}

fn gen_test_decimals() -> Vec<MyDecimal> {
    let mut rng = Rng(0x2026_1104);
    let mut test_dec = Vec::with_capacity(NUM_TEST_DEC);
    for _ in 0..NUM_TEST_DEC {
        let mut f = rng.float64();
        let digits = rng.int() % 12 + 1;
        let offset = rng.int() % digits + 1;
        f = (f * 10f64.powi(digits as i32)).round() / 10f64.powi((digits - offset) as i32);
        test_dec.push(new_dec_from_float_for_test(f));
    }
    test_dec
}

fn benchmark_round(c: &mut Criterion) {
    let mut round_to = MyDecimal::default();
    let tests: Vec<(&str, isize)> = vec![
        ("123456789.987654321", 1),
        ("15.1", 0),
        ("15.5", 0),
        ("15.9", 0),
        ("-15.1", 0),
        ("-15.5", 0),
        ("-15.9", 0),
        ("15.1", 1),
        ("-15.1", 1),
        ("15.17", 1),
        ("15.4", -1),
        ("-15.4", -1),
        ("5.4", -1),
        (".999", 0),
        ("999999999", -9),
    ];
    let inputs: Vec<(MyDecimal, isize)> = tests
        .iter()
        .map(|(s, scale)| {
            let mut d = MyDecimal::default();
            d.from_string(s.as_bytes()).unwrap();
            (d, *scale)
        })
        .collect();
    c.bench_function("BenchmarkRound", |b| {
        b.iter(|| {
            for mode in [MODE_HALF_UP, MODE_TRUNCATE, MODE_CEILING] {
                for (d, scale) in &inputs {
                    black_box(d).round(&mut round_to, *scale, mode).unwrap();
                }
            }
        })
    });
}

fn benchmark_to_float64(c: &mut Criterion) {
    let test_dec = gen_test_decimals();
    c.bench_function("BenchmarkToFloat64New", |b| {
        b.iter(|| {
            for d in &test_dec {
                let (f, _) = black_box(d).to_float64();
                black_box(f);
            }
        })
    });
    c.bench_function("BenchmarkToFloat64Old", |b| {
        b.iter(|| {
            for d in &test_dec {
                // Go: strconv.ParseFloat(testDec[j].String(), 64)
                let (f, err) = gostd::parse_float(&black_box(d).string());
                let _ = black_box((f, err));
            }
        })
    });
}

/// Not in Go's benchmark file: FromFloat64 (`strconv.FormatFloat(f, 'g', -1, 64)` + FromString)
/// on 1000 random bit-pattern doubles and on 1000 values `k*0.1+0.2` (typically 17 digits).
fn benchmark_from_float64(c: &mut Criterion) {
    let mut rng = Rng(0x2026_1106);
    let mut random_bits = Vec::with_capacity(NUM_TEST_DEC);
    while random_bits.len() < NUM_TEST_DEC {
        let f = f64::from_bits(rng.next_u64());
        if f.is_finite() {
            random_bits.push(f);
        }
    }
    let tenths: Vec<f64> = (0..NUM_TEST_DEC).map(|k| k as f64 * 0.1 + 0.2).collect();
    for (name, input) in [
        ("BenchmarkFromFloat64RandomBits", &random_bits),
        ("BenchmarkFromFloat64Tenths", &tenths),
    ] {
        c.bench_function(name, |b| {
            b.iter(|| {
                for f in input {
                    let mut d = MyDecimal::default();
                    let err = d.from_float64(black_box(*f));
                    let _ = black_box((d, err));
                }
            })
        });
    }
}

fn benchmark_my_decimal_to_bin_or_hash_cases() -> Vec<MyDecimal> {
    [
        "1.000000000000",
        "3",
        "12.000000000",
        "120",
        "120000",
        "100000000000.00000",
        "0.000000001200000000",
        "98765.4321",
        "-123.456000000000000000",
        "0",
        "0000000000",
        "0.00000000000",
    ]
    .iter()
    .map(|s| {
        let mut d = MyDecimal::default();
        d.from_string(s.as_bytes()).unwrap();
        d
    })
    .collect()
}

fn benchmark_my_decimal_to_bin(c: &mut Criterion) {
    let decs = benchmark_my_decimal_to_bin_or_hash_cases();
    c.bench_function("BenchmarkMyDecimalToBin", |b| {
        b.iter(|| {
            for dec in &decs {
                let (prec, frac) = black_box(dec).precision_and_frac();
                let (bin, err) = dec.to_bin(prec, frac);
                err.unwrap();
                black_box(bin);
            }
        })
    });
}

fn benchmark_my_decimal_to_hash_key(c: &mut Criterion) {
    let decs = benchmark_my_decimal_to_bin_or_hash_cases();
    c.bench_function("BenchmarkMyDecimalToHashKey", |b| {
        b.iter(|| {
            for dec in &decs {
                let (key, err) = black_box(dec).to_hash_key();
                err.unwrap();
                black_box(key);
            }
        })
    });
}

criterion_group!(
    benches,
    benchmark_round,
    benchmark_to_float64,
    benchmark_from_float64,
    benchmark_my_decimal_to_bin,
    benchmark_my_decimal_to_hash_key
);
criterion_main!(benches);
