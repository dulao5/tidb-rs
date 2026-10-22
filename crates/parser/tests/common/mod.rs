//! Test corpus: cases extracted from TiDB's `pkg/parser/parser_test.go` by
//! `tools/parser-port/extract_tests.py` (see the provenance record on the first line).
#![allow(dead_code)]

pub struct Case {
    /// Line in parser_test.go.
    pub line: u64,
    pub src: String,
    /// Whether TiDB accepts the statement.
    pub ok: bool,
    /// The enclosing Go test enables window functions.
    pub window: bool,
    /// "default" | "mariadb" | "real_as_float" | "other" (custom parser setup).
    pub mode: String,
    /// Expected `Restore()` output, when the Go test gives one.
    pub restore: Option<String>,
}

pub fn load() -> Vec<Case> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/all_cases.jsonl");
    let text = std::fs::read_to_string(path).expect("read corpus");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("corpus line is JSON"))
        .filter(|v| v.get("src").is_some()) // skip the provenance record
        .map(|v| Case {
            line: v["line"].as_u64().unwrap(),
            src: v["src"].as_str().unwrap().to_string(),
            ok: v["ok"].as_bool().unwrap(),
            window: v["window"].as_bool().unwrap(),
            mode: v["mode"].as_str().unwrap().to_string(),
            restore: v["restore"].as_str().map(str::to_string),
        })
        .collect()
}

/// Cases run with the default parser configuration (the only mode supported so far).
pub fn default_mode() -> Vec<Case> {
    load().into_iter().filter(|c| c.mode == "default").collect()
}
