//! Development tool: parse SQL and print its `Restore()` text.
//!
//! ```text
//! cargo run -p tidb-parser --example restore -- "SELECT ..." ["SET ..." ...]
//! cargo run -p tidb-parser --example restore -- --corpus [FILTER]
//! ```
//!
//! `--corpus` restores every accepted default-mode case of `tests/data/all_cases.jsonl` that has
//! an expected restore text and prints the cases whose status (`ok`, `MISMATCH`, or the
//! `unported ...` reason) contains FILTER (all cases when FILTER is omitted).
use tidb_parser::{ParseOptions, ast::Stmt, parse};

fn restore_all(stmts: &[Stmt]) -> Result<String, String> {
    let mut out = String::new();
    for (i, s) in stmts.iter().enumerate() {
        if i != 0 {
            out.push_str("; ");
        }
        s.restore(&mut out)?;
    }
    Ok(out)
}

fn corpus(filter: &str) {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/all_cases.jsonl");
    let text = std::fs::read_to_string(path).expect("read corpus");
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("corpus line is JSON");
        if v.get("src").is_none() || v["mode"] != "default" || v["ok"] != true {
            continue;
        }
        let Some(want) = v["restore"].as_str() else {
            continue;
        };
        let src = v["src"].as_str().unwrap();
        let opts = ParseOptions {
            window_functions: v["window"].as_bool().unwrap(),
        };
        let (tag, got) = match parse(src, &opts).map(|o| restore_all(&o.stmts)) {
            Err(e) => (format!("parse error: {e}"), String::new()),
            Ok(Err(why)) => (why, String::new()),
            Ok(Ok(got)) if got == want => ("ok".to_string(), got),
            Ok(Ok(got)) => ("MISMATCH".to_string(), got),
        };
        if tag.contains(filter) {
            println!("{tag}\n  src : {src}\n  want: {want}\n  got : {got}");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--corpus") {
        corpus(args.get(1).map_or("", String::as_str));
        return;
    }
    for sql in &args {
        match parse(sql, &ParseOptions::default()) {
            Err(e) => println!("{sql}\n  error: {e}"),
            Ok(out) => match restore_all(&out.stmts) {
                Ok(r) => println!("{sql}\n  => {r}"),
                Err(why) => println!("{sql}\n  restore failed: {why}"),
            },
        }
    }
}
