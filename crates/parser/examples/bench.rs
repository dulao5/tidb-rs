//! Throughput benchmark over the parser_test.go corpus (same method as the spike).
//!
//! ```text
//! cargo run --release -p tidb-parser --example bench [seconds per measurement, default 3]
//! ```
//!
//! * single-call: every accepted default-mode statement parsed with its own `parse` call;
//! * batched x50: the same statements joined into `;`-separated chunks of 50.
use std::{hint::black_box, time::Instant};

use tidb_parser::{ParseOptions, Parser, TABLE_STATS};

struct Case {
    src: String,
    window: bool,
}

fn load() -> Vec<Case> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/all_cases.jsonl");
    std::fs::read_to_string(path)
        .expect("read corpus")
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("src").is_some() && v["mode"] == "default" && v["ok"] == true)
        .map(|v| Case {
            src: v["src"].as_str().unwrap().to_string(),
            window: v["window"].as_bool().unwrap(),
        })
        .collect()
}

/// Run `f` over all items repeatedly for `secs`; returns items/s.
fn run<T>(items: &[T], secs: f64, mut f: impl FnMut(&T)) -> f64 {
    let t0 = Instant::now();
    let mut rounds = 0u64;
    while t0.elapsed().as_secs_f64() < secs {
        items.iter().for_each(&mut f);
        rounds += 1;
    }
    rounds as f64 * items.len() as f64 / t0.elapsed().as_secs_f64()
}

fn main() {
    let secs: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; use --release for meaningful numbers");
    }
    println!("tables: {TABLE_STATS}");
    let mut p = Parser::new();
    let opts = |w: bool| ParseOptions {
        window_functions: w,
    };
    let cases: Vec<Case> = load()
        .into_iter()
        .filter(|c| p.parse(&c.src, &opts(c.window)).is_ok())
        .collect();
    let bytes: usize = cases.iter().map(|c| c.src.len()).sum();
    println!("corpus: {} accepted statements, {bytes} bytes", cases.len());

    let sps = run(&cases, secs, |c| {
        black_box(p.parse(&c.src, &opts(c.window)).ok());
    });
    println!(
        "single-call : {sps:>10.0} stmts/s  {:>6.1} MB/s",
        sps * bytes as f64 / cases.len() as f64 / 1e6
    );

    let mut chunks: Vec<(String, bool, usize)> = Vec::new();
    for win in [false, true] {
        let sel: Vec<&Case> = cases.iter().filter(|c| c.window == win).collect();
        for ch in sel.chunks(50) {
            let joined = ch
                .iter()
                .map(|c| c.src.trim_end().trim_end_matches(';'))
                .collect::<Vec<_>>()
                .join(";\n");
            if p.parse(&joined, &opts(win)).is_ok() {
                chunks.push((joined, win, ch.len()));
            }
        }
    }
    let n_stmts: usize = chunks.iter().map(|c| c.2).sum();
    let cps = run(&chunks, secs, |(src, win, _)| {
        black_box(p.parse(src, &opts(*win)).ok());
    });
    println!(
        "batched x50 : {:>10.0} stmts/s  ({} chunks, {n_stmts} stmts)",
        cps * n_stmts as f64 / chunks.len() as f64,
        chunks.len()
    );
}
