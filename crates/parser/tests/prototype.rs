//! Prototype workload gate: every statement of `tests/data/prototype.sql` (sysbench
//! oltp_read_only and the statements MySQL clients/drivers send around a session) must be inside
//! the ported subset and restore byte-identically to TiDB's `Restore()` text, recorded by the Go
//! parser in `tests/data/prototype.jsonl` (see the header of `prototype.sql`).
use tidb_parser::{ParseOptions, parse};

#[test]
fn prototype_statements_restore_like_tidb() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/prototype.jsonl");
    let text = std::fs::read_to_string(path).expect("read prototype.jsonl");
    let mut failures = Vec::new();
    let mut n = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("JSON line");
        let Some(src) = v["src"].as_str() else {
            continue; // provenance record
        };
        n += 1;
        let want = v["restore"]
            .as_str()
            .unwrap_or_else(|| panic!("TiDB rejects {src:?}: {}", v["error"]));
        let got = parse(src, &ParseOptions::default())
            .map_err(|e| format!("parse error: {e}"))
            .and_then(|o| {
                let mut out = String::new();
                for (i, s) in o.stmts.iter().enumerate() {
                    if i != 0 {
                        out.push_str("; ");
                    }
                    s.restore(&mut out)?;
                }
                Ok(out)
            });
        if got.as_deref() != Ok(want) {
            failures.push(format!("src : {src}\nwant: {want}\ngot : {got:?}"));
        }
    }
    assert!(n >= 80, "only {n} prototype statements");
    assert!(
        failures.is_empty(),
        "{} of {n} prototype statements differ from TiDB:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
