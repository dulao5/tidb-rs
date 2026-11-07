//! `Unix(sec, 0).In(loc).Date()`: the zone lookup of a TIMESTAMP conversion (per row).
//! Go: `BenchmarkDateIn` in the D01.3 follow-up PR description (same TZif bytes).

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tidb_base::gostd::time::{self as gotime, load_location_from_tzdata};

fn bench(c: &mut Criterion) {
    // 2026-11-07 12:00 UTC, inside the zone of "now" while the benchmark runs in 2026-27;
    // and 2010-06-01, outside it (the binary search, or tzset for the slim file).
    let near = 1_794_052_800i64;
    let far = 1_275_350_400i64;
    for (kind, data) in [
        (
            "fat",
            &include_bytes!("../tests/testdata/America_New_York.fat")[..],
        ),
        (
            "slim",
            &include_bytes!("../tests/testdata/America_New_York.slim")[..],
        ),
    ] {
        let loc = std::sync::Arc::new(load_location_from_tzdata("America/New_York", data).unwrap());
        for (when, sec) in [("now", near), ("2010", far)] {
            c.bench_function(&format!("date_in/{kind}/{when}"), |b| {
                b.iter(|| gotime::unix(black_box(sec), 0).in_loc(&loc).date())
            });
            c.bench_function(&format!("lookup/{kind}/{when}"), |b| {
                b.iter(|| loc.lookup(black_box(sec)).offset)
            });
        }
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
