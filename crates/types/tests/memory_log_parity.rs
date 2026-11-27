//! Whole-line golden parity tests for D00.3 Batch 1 structured logs in `tidb-memory`
//! (`meminfo`, `sqlkiller`, `action`) through the real `tidb-log` text encoder (`MemorySink`).
//!
//! Lives in `tidb-types` (rank 2) because `tidb-memory` and `tidb-log` are both rank 1 and
//! `scripts/check_layers.py` forbids same-rank dev-dependencies. Dedicated integration test
//! binary (installs global `MemorySink`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tidb_base::config::LogConfig;
use tidb_log::sink::MemorySink;
use tidb_memory::action::{ActionOnExceed, LogOnExceed, PanicOnExceed};
use tidb_memory::meminfo;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_memory::tracker::new_tracker;

fn normalize_caller(line: &str) -> String {
    assert!(
        !line.contains('\n') && !line.contains('\r'),
        "log line must be single-line: {line:?}"
    );
    let Some((level_part, rest)) = line.split_once("] [") else {
        return line.to_owned();
    };
    let Some((caller, tail)) = rest.split_once("] ") else {
        return line.to_owned();
    };
    let Some((file, line_no)) = caller.rsplit_once(':') else {
        return line.to_owned();
    };
    assert!(
        !line_no.is_empty() && line_no.bytes().all(|b| b.is_ascii_digit()),
        "expected numeric caller line in {line:?}"
    );
    format!("{level_part}] [{file}:<LINE>] {tail}")
}

fn normalize_int_field(line: &str, key: &str) -> String {
    let needle = format!("[{key}=");
    let Some((head, tail)) = line.split_once(&needle) else {
        return line.to_owned();
    };
    let Some((val, rest)) = tail.split_once(']') else {
        return line.to_owned();
    };
    let digits = val.strip_prefix('-').unwrap_or(val);
    assert!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "expected integer value for {key} in {line:?}"
    );
    format!("{head}[{key}=<N>]{rest}")
}

#[test]
fn memory_structured_logs_match_go_whole_line() {
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

    // 1. init_memory_hook: normalize only caller line and the two host memory byte counts.
    meminfo::init_memory_hook();
    let init_out = String::from_utf8(sink.take()).unwrap();
    let init_lines: Vec<String> = init_out
        .lines()
        .map(normalize_caller)
        .map(|l| normalize_int_field(&l, "cgroupMemorySize"))
        .map(|l| normalize_int_field(&l, "physicalMemorySize"))
        .collect();
    assert!(!init_lines.is_empty(), "init_memory_hook emitted no logs");
    for line in &init_lines {
        assert!(
            line == "[WARN] [memory_limit.rs:<LINE>] [\"no cgroup memory controller detected\"]"
                || line
                    == "[INFO] [meminfo.rs:<LINE>] [\"use cgroup memory hook because TiDB is in the container\"]"
                || line
                    == "[INFO] [meminfo.rs:<LINE>] [\"use cgroup memory hook\"] [cgroupMemorySize=<N>] [physicalMemorySize=<N>]"
                || line
                    == "[INFO] [meminfo.rs:<LINE>] [\"use physical memory hook\"] [cgroupMemorySize=<N>] [physicalMemorySize=<N>]",
            "unexpected init_memory_hook line: {line}"
        );
    }

    // 2. LogOnExceed: exact whole-line Go WARN `"memory exceeds quota"` with `[error=...]`,
    //    verifying single-line `\n` and `\"` escaping through the real text encoder.
    let t = new_tracker(1, 1);
    let log_action = LogOnExceed::new(42);
    log_action.action(&t).unwrap();
    let log_out = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(log_out.trim_end_matches('\n')),
        "[WARN] [action.rs:<LINE>] [\"memory exceeds quota\"] [error=\"[util:8001]%!s(int=1) holds 0B memory, exceeds threshold 1B.\\n\\\"1\\\"{\\n  \\\"quota\\\": 1 Bytes\\n  \\\"consumed\\\": 0 Bytes\\n}\\n\"]"
    );

    // 3. PanicOnExceed + SqlKiller: exact whole-line assertions including key quoting
    //    `["connection ID"=42]` through the real text encoder.
    let killer = SqlKiller::new();
    killer.conn_id.store(42, Ordering::SeqCst);
    let t2 = new_tracker(1, 1);
    t2.session_id.store(42, Ordering::SeqCst);
    let panic_action = PanicOnExceed::new(Some(Arc::clone(&killer)), 42);
    assert!(panic_action.action(&t2).is_err());
    killer.reset();

    let kill_out = String::from_utf8(sink.take()).unwrap();
    let kill_lines: Vec<String> = kill_out.lines().map(normalize_caller).collect();
    assert_eq!(
        kill_lines,
        vec![
            "[WARN] [action.rs:<LINE>] [\"memory exceeds quota\"] [conn=42] [error=\"[util:8001]%!s(int=1) holds 0B memory, exceeds threshold 1B.\\n\\\"1\\\"{\\n  \\\"quota\\\": 1 Bytes\\n  \\\"consumed\\\": 0 Bytes\\n}\\n\"]",
            "[WARN] [sqlkiller.rs:<LINE>] [\"kill initiated\"] [\"connection ID\"=42] [reason=\"[executor:8175]Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=42]\"]",
            "[WARN] [sqlkiller.rs:<LINE>] [\"kill finished\"] [conn=42]",
        ]
    );
}
