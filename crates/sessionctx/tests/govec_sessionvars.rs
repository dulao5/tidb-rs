//! Replay of the govec domain "sessionvars" (block D05.2): Go TiDB v8.5.8 vectors.
//!
//! T2: op `fresh`, the scalar fields of a fresh `NewSessionVars(nil)` against
//! `SessionVarsFields::default()`. The set / get / hook ops come with T3-T5.

use std::collections::BTreeMap;
use std::io::Read;

use serde_json::Value;
use tidb_sessionctx::variable::session_fields::SCALAR_FIELDS;
use tidb_sessionctx::variable::session_fields::SessionVarsFields;

const GO_TARGET: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else if cfg!(target_arch = "aarch64") {
    "arm64"
} else {
    "unsupported"
};

fn replay(path: &str) -> usize {
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).expect("vectors"));
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    let mut lines = text.lines();
    let header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!(header["domain"], "sessionvars");
    let mut fails = vec![];
    let mut n = 0;
    for line in lines {
        n += 1;
        let rec: Value = serde_json::from_str(line).unwrap();
        match rec["op"].as_str().unwrap() {
            "fresh" => {
                let want: BTreeMap<String, String> = rec["out"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect();
                let got: BTreeMap<String, String> = SessionVarsFields::default()
                    .scalar_strings()
                    .into_iter()
                    .map(|(k, v)| (k.to_owned(), v))
                    .collect();
                if got.len() != SCALAR_FIELDS.len() {
                    fails.push(format!("duplicate field names: {}", got.len()));
                }
                for (k, w) in &want {
                    match got.get(k) {
                        Some(g) if g == w => {}
                        g => fails.push(format!("fresh {k}: got {g:?}, want {w:?}")),
                    }
                }
                for k in got.keys().filter(|k| !want.contains_key(*k)) {
                    fails.push(format!("fresh {k}: not a scalar field in Go"));
                }
            }
            op => fails.push(format!("unknown op {op}")),
        }
    }
    assert!(
        fails.is_empty(),
        "{} mismatches:\n{}",
        fails.len(),
        fails.join("\n")
    );
    n
}

#[test]
fn govec_sessionvars() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/v8.5.8");
    let path = format!("{dir}/{GO_TARGET}/sessionvars.jsonl.gz");
    if !std::path::Path::new(&path).exists() {
        panic!("no vectors for {GO_TARGET}: {path}");
    }
    assert!(replay(&path) >= 1);
}
