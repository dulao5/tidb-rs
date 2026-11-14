//! Replays the govec `preprocess` vectors (tidb-rs-tracker `govec/domains/preprocess.go`):
//! block D09.0, the VS1-10 part of `preprocess.go` — the SELECT checks of
//! `preprocess_test.go` `TestValidator` and more — against Go TiDB v8.5.8.
//!
//! Records whose Rust result is a boundary stub (`blocked-by:<block>`: resolving a base
//! table needs the InfoSchema interface of D06.3 part 2, VS1-17) are counted as blocked
//! (constitution §7.3); the count per block is pinned below, so a stub that starts answering
//! for records it should not cannot stay green. A record Go parses must parse in Rust. Every
//! other record must match Go's error code and message (or Go's success).

use std::collections::BTreeMap;
use std::io::Read;

use serde_json::{Value, json};
use tidb_base::Error;
use tidb_base::blocked::blocked_by;
use tidb_parser::{ParseOptions, parse};
use tidb_planner::core::preprocess::{
    PreprocessOpt, PreprocessorReturn, in_prepare, preprocess, with_preprocessor_return,
};
use tidb_planner::core::resolve::new_node_w;
use tidb_sessionctx::variable::session::{GlobalVarAccessor, SessionVars};

/// The number of records answered by each boundary stub.
const EXPECTED_BLOCKED: [(&str, usize); 3] = [("D02.2", 4), ("D06.3", 11), ("D09.5", 3)];

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

struct Acc;

impl GlobalVarAccessor for Acc {
    fn get_global_sys_var(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_global_sys_var_only(&mut self, _: &str, _: &str, _: bool) -> Result<(), Error> {
        Ok(())
    }
    fn get_tidb_table_value(&self, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    fn set_tidb_table_value(&mut self, _: &str, _: &str, _: &str) -> Result<(), Error> {
        Ok(())
    }
}

enum Outcome {
    Value(Value),
    Unparsed,
    Blocked(String),
}

fn run(sql: &str, prepare: bool) -> Outcome {
    let Ok(out) = parse(sql, &ParseOptions::default()) else {
        return Outcome::Unparsed;
    };
    let Some(mut stmt) = out.stmts.into_iter().next() else {
        return Outcome::Unparsed;
    };
    let mut vars = SessionVars::new(Box::new(Acc));
    vars.current_db = "test".to_string();
    let mut node = new_node_w(&mut stmt);
    let mut opts: Vec<PreprocessOpt> = Vec::new();
    if prepare {
        opts.push(Box::new(in_prepare));
    }
    opts.push(with_preprocessor_return(PreprocessorReturn {
        has_info_schema: true,
        ..Default::default()
    }));
    let (_, res) = preprocess(&mut vars, &mut node, opts);
    match res {
        Ok(()) => Outcome::Value(Value::Null),
        Err(e) => {
            if let Some(b) = blocked_by(&e) {
                return Outcome::Blocked(b.to_string());
            }
            Outcome::Value(json!({"code": e.code(), "msg": String::from_utf8_lossy(&e.error())}))
        }
    }
}

#[test]
fn govec_preprocess() {
    let path = format!(
        "{}/tests/vectors/v8.5.8/{GO_TARGET}/preprocess.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(&path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "preprocess");
    let (mut matched, mut unparsed, mut fails) = (0, Vec::new(), Vec::new());
    let mut blocked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        let sql = rec["args"][0].as_str().unwrap();
        if rec["out"].get("parse").is_some() {
            // Go does not parse it either: nothing to compare.
            continue;
        }
        match run(sql, rec["args"][1].as_bool().unwrap()) {
            Outcome::Unparsed => unparsed.push(sql.to_string()),
            Outcome::Blocked(b) => blocked.entry(b).or_default().push(sql.to_string()),
            Outcome::Value(got) if got == rec["out"] => matched += 1,
            Outcome::Value(got) => fails.push(format!("{line}\n    got {got}")),
        }
    }
    assert_eq!(n as u64, header["records"].as_u64().unwrap());
    eprintln!(
        "preprocess: {matched} matched, {} blocked ({}), {} not parsed by the Rust parser",
        blocked.values().map(Vec::len).sum::<usize>(),
        blocked
            .iter()
            .map(|(b, v)| format!("{b}: {}", v.len()))
            .collect::<Vec<_>>()
            .join(", "),
        unparsed.len()
    );
    assert!(
        fails.is_empty(),
        "{} of {n} records differ:\n{}",
        fails.len(),
        fails.join("\n")
    );
    assert!(
        unparsed.is_empty(),
        "Go parses, Rust does not: {unparsed:?}"
    );
    // The records each boundary stub answers today; change with the stub (and the vectors).
    let counts: BTreeMap<&str, usize> =
        blocked.iter().map(|(b, v)| (b.as_str(), v.len())).collect();
    assert_eq!(
        counts,
        BTreeMap::from(EXPECTED_BLOCKED),
        "blocked records: {blocked:?}"
    );
}
