//! Whole-line golden parity tests for D00.3 Batch 3 structured logs in `tidb-executor`
//! (`consistency::Reporter::report_lookup_inconsistent`, including `[stack="..."]`).
//! Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;

use tidb_base::config::LogConfig;
use tidb_base::errors::{RedactLogDisable, RedactLogEnable};
use tidb_executor::consistency::Reporter;
use tidb_kv::key::{Handle, IntHandle};
use tidb_log::sink::MemorySink;
use tidb_model::{IndexInfo, TableInfo};
use tidb_parser::model::CIStr;

fn normalize_caller_and_stack(line: &str) -> String {
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
    // `release-checked` builds with `debug = false`, so `Backtrace::force_capture` emits
    // symbolized frames separated by `\n` without `\n\t<file>:<line>` source locations.
    assert!(
        !stack_body.is_empty()
            && stack_body.contains("\\n")
            && (!cfg!(debug_assertions) || stack_body.contains("\\n\\t")),
        "expected non-empty backtrace frames (with \\n\\t under debug): {stack_body:?}"
    );
    let normalized_prefix = if let Some((head, tail)) = prefix.split_once("[consistency.rs:")
        && let Some((line_no, rest)) = tail.split_once(']')
        && !line_no.is_empty()
        && line_no.bytes().all(|b| b.is_ascii_digit())
    {
        format!("{head}[consistency.rs:<LINE>]{rest}")
    } else {
        prefix.to_owned()
    };
    format!("{normalized_prefix} [stack=\"<STACK>\"]")
}

#[test]
fn report_lookup_inconsistent_matches_go_whole_line() {
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

    let tbl = TableInfo {
        name: CIStr::new("t1"),
        ..TableInfo::default()
    };
    let idx = IndexInfo {
        name: CIStr::new("idx_a"),
        ..IndexInfo::default()
    };
    let miss = vec![Handle::Int(IntHandle(2))];
    let full = vec![Handle::Int(IntHandle(1)), Handle::Int(IntHandle(2))];

    let rep_off = Reporter {
        tbl: &tbl,
        idx: &idx,
        enable_redact_log: RedactLogDisable.to_owned(),
    };
    let err = rep_off.report_lookup_inconsistent(2, 1, &miss, &full, 1);
    assert!(err.to_string().contains("t1"));
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller_and_stack(line.trim_end_matches('\n')),
        "[ERROR] [consistency.rs:<LINE>] [\"indexLookup found data inconsistency\"] [table_name=t1] [index_name=idx_a] [index_cnt=2] [table_cnt=1] [missing_handles=\"[2]\"] [total_handles=\"[1 2]\"] [stack=\"<STACK>\"]"
    );

    let rep_on = Reporter {
        tbl: &tbl,
        idx: &idx,
        enable_redact_log: RedactLogEnable.to_owned(),
    };
    let _ = rep_on.report_lookup_inconsistent(2, 1, &miss, &full, 1);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller_and_stack(line.trim_end_matches('\n')),
        "[ERROR] [consistency.rs:<LINE>] [\"indexLookup found data inconsistency\"] [table_name=t1] [index_name=idx_a] [index_cnt=2] [table_cnt=1] [missing_handles=] [total_handles=] [stack=\"<STACK>\"]"
    );
}
