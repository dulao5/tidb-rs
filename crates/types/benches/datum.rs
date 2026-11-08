//! Datum hot operations, the same loop bodies as the Go benchmark in the tracker
//! (`govec/domains/datum_bench_test.go`, `go test -bench Datum -run '^$' ./domains/`).
//!
//! Rust: `cargo bench -p tidb-types --bench datum`

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use tidb_base::mysql;
use tidb_types::collate;
use tidb_types::context::DefaultStmtNoWarningContext;
use tidb_types::datum::*;
use tidb_types::field_type::new_field_type;
use tidb_types::field_type_builder::new_field_type_builder;
use tidb_types::mydecimal::MyDecimal;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn bench(c: &mut Criterion) {
    collate::set_new_collation_enabled_for_test(true);
    let ctx = &*DefaultStmtNoWarningContext;
    let bin = collate::get_collator("utf8mb4_bin");
    let ci = collate::get_collator("utf8mb4_general_ci");

    let a = new_int_datum(12345);
    let b = new_int_datum(12346);
    c.bench_function("compare_int", |bn| {
        bn.iter(|| black_box(black_box(&a).compare(ctx, black_box(&b), bin).0))
    });
    let s1 = new_collation_string_datum(b"hello world, tidb".to_vec(), "utf8mb4_bin");
    let s2 = new_collation_string_datum(b"hello world, tidc".to_vec(), "utf8mb4_bin");
    c.bench_function("compare_string_bin", |bn| {
        bn.iter(|| black_box(black_box(&s1).compare(ctx, black_box(&s2), bin).0))
    });
    c.bench_function("compare_string_general_ci", |bn| {
        bn.iter(|| black_box(black_box(&s1).compare(ctx, black_box(&s2), ci).0))
    });
    let varchar = new_field_type_builder()
        .set_type(mysql::TypeVarchar)
        .set_flen(32)
        .set_charset("utf8mb4")
        .set_collate("utf8mb4_bin")
        .build_p();
    c.bench_function("convert_int_to_varchar", |bn| {
        bn.iter(|| black_box(black_box(&a).convert_to(ctx, &varchar).0))
    });
    let bigint = new_field_type(mysql::TypeLonglong);
    let ns = new_string_datum(b"123456789".to_vec());
    c.bench_function("convert_string_to_bigint", |bn| {
        bn.iter(|| black_box(black_box(&ns).convert_to(ctx, &bigint).0))
    });
    let dec = new_field_type_builder()
        .set_type(mysql::TypeNewDecimal)
        .set_flen(10)
        .set_decimal(2)
        .build_p();
    let f = new_float64_datum(12345.678);
    c.bench_function("convert_float_to_decimal", |bn| {
        bn.iter(|| black_box(black_box(&f).convert_to(ctx, &dec).0))
    });
    let mut d = MyDecimal::default();
    d.from_string(b"12345.678").unwrap();
    let dd = new_decimal_datum(d);
    c.bench_function("decimal_to_int64", |bn| {
        bn.iter(|| black_box(black_box(&dd).to_int64(ctx).0))
    });
    c.bench_function("clone_string", |bn| {
        bn.iter(|| black_box(black_box(&s1).clone_datum()))
    });
    c.bench_function("set_string", |bn| {
        let mut x = Datum::default();
        bn.iter(|| {
            x.set_string(black_box(b"hello world, tidb".to_vec()), "utf8mb4_bin");
            black_box(&x);
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
