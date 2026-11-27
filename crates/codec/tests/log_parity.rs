//! Whole-line golden parity test for D00.3 Batch 1 structured log in `tidb-codec::hash_code`.
//! Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;

use tidb_base::config::LogConfig;
use tidb_log::sink::MemorySink;
use tidb_types::datum::{Datum, GoValue};

#[test]
fn hash_code_unexpected_datum_kind_matches_go_whole_line() {
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "debug".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    let log_sink = Arc::clone(&sink);
    tidb_log::init_logger_with_sinks(&cfg, "", log_sink, None).unwrap();

    let mut d = Datum::default();
    d.set_interface(GoValue::Nil);
    let mut out = Vec::new();
    tidb_codec::hash_code(&mut out, &d);

    let raw = String::from_utf8(sink.take()).unwrap();
    let line = raw.trim_end_matches('\n');
    assert!(
        !line.contains('\n') && !line.contains('\r'),
        "log line must be single-line: {line:?}"
    );
    let (prefix, stack_quoted) = line
        .split_once(" [stack=\"")
        .unwrap_or_else(|| panic!("missing [stack=\"...\"] field: {line}"));
    let stack_body = stack_quoted
        .strip_suffix("\"]")
        .unwrap_or_else(|| panic!("stack must be final field: {line}"));
    assert!(
        !stack_body.is_empty() && stack_body.contains("\\n\\t"),
        "expected non-empty Go-style \\n\\t stack frames: {stack_body:?}"
    );
    let normalized_prefix = if let Some((head, tail)) = prefix.split_once("[codec.rs:")
        && let Some((line_no, rest)) = tail.split_once(']')
        && !line_no.is_empty()
        && line_no.bytes().all(|b| b.is_ascii_digit())
    {
        format!("{head}[codec.rs:<LINE>]{rest}")
    } else {
        prefix.to_owned()
    };
    assert_eq!(
        format!("{normalized_prefix} [stack=\"<STACK>\"]"),
        "[WARN] [codec.rs:<LINE>] [\"trying to calculate HashCode of an unexpected type of Datum\"] [\"Datum Kind\"=14] [stack=\"<STACK>\"]"
    );
}
