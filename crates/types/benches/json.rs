//! JSON benchmarks of the A4 spec (6.4): parse, text output, constant-path Extract, compare
//! and hash on the same documents as the Go benchmark in the tracker
//! (`govec/domains/json_bench_test.go`).
//!
//! Go: `cd govec && go test -bench . -run '^$' ./domains/`
//! Rust: `cargo bench -p tidb-types --bench json`

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tidb_types::json::{self as j, BinaryJSON};

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// The documents (same text in the Go benchmark): ~100 B and ~4 KB.
fn small() -> String {
    r#"{"id": 12345, "name": "tidb-rs", "tags": ["a", "b", "c"], "score": 3.5, "ok": true}"#
        .to_owned()
}

fn medium() -> String {
    let mut s = String::from("{");
    for i in 0..40 {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#""key{i:02}": {{"id": {i}, "name": "item number {i}", "vals": [1, 2.5, "x", null, false], "nested": {{"a": {i}}}}}"#
        ));
    }
    s.push('}');
    s
}

/// ~256 KB: an array of 64 medium documents.
fn large() -> String {
    let md = medium();
    format!("[{}]", vec![md; 64].join(", "))
}

fn bench(c: &mut Criterion) {
    let (sm, md) = (small(), medium());
    let bj_md: BinaryJSON<'static> = j::parse_binary_json_from_string(md.as_bytes()).unwrap();
    let bj_sm: BinaryJSON<'static> = j::parse_binary_json_from_string(sm.as_bytes()).unwrap();
    let path = j::parse_json_path_expr(b"$.key20.nested.a").unwrap();
    c.bench_function("parse_small", |b| {
        b.iter(|| j::parse_binary_json_from_string(black_box(sm.as_bytes())).unwrap())
    });
    c.bench_function("parse_medium", |b| {
        b.iter(|| j::parse_binary_json_from_string(black_box(md.as_bytes())).unwrap())
    });
    let lg = large();
    let mut g = c.benchmark_group("large");
    g.throughput(criterion::Throughput::Bytes(lg.len() as u64));
    g.bench_function("parse_large", |b| {
        b.iter(|| j::parse_binary_json_from_string(black_box(lg.as_bytes())).unwrap())
    });
    g.finish();
    c.bench_function("string_medium", |b| b.iter(|| black_box(&bj_md).string()));
    c.bench_function("extract_const_path", |b| {
        b.iter(|| black_box(&bj_md).extract(std::slice::from_ref(&path)))
    });
    // tidb-rs has no process-wide path cache: compare with Go's parse_path_uncached.
    c.bench_function("parse_path", |b| {
        b.iter(|| j::parse_json_path_expr(black_box(b"$.key20.nested.a")).unwrap())
    });
    c.bench_function("compare_medium", |b| {
        b.iter(|| j::compare_binary_json(black_box(bj_md.as_ref()), black_box(bj_md.as_ref())))
    });
    c.bench_function("hash_small", |b| {
        b.iter(|| black_box(&bj_sm).hash_value(Vec::new()))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
