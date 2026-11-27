//! Whole-line golden parity tests for D00.3 Batch 1 structured logs in `tidb-runtime`
//! (`ExecPool` stall warning and `spawn_with_recover` panic recovery) through the real
//! `tidb-log` text encoder (`MemorySink`) and global `tidb_log::set_panic_hook()`.
//!
//! Lives in `tidb-types` (rank 2) because `tidb-runtime` and `tidb-log` are both rank 1 and
//! `scripts/check_layers.py` forbids same-rank dev-dependencies. Dedicated integration test
//! binary (installs global `MemorySink` and global panic hook).

use std::sync::Arc;
use std::time::Duration;

use tidb_base::config::LogConfig;
use tidb_log::sink::MemorySink;
use tidb_runtime::{Runtime, RuntimeConfig, SliceConfig};

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

fn normalize_field(line: &str, key: &str, placeholder: &str) -> String {
    let needle = format!("[{key}=");
    let Some((head, tail)) = line.split_once(&needle) else {
        return line.to_owned();
    };
    let Some((val, rest)) = tail.split_once(']') else {
        return line.to_owned();
    };
    assert!(!val.is_empty(), "empty value for {key} in {line:?}");
    format!("{head}[{key}={placeholder}]{rest}")
}

fn normalize_stack_trace(line: &str) -> String {
    let base = normalize_caller(line);
    let (prefix, stack_quoted) = base
        .split_once(" [\"stack trace\"=\"")
        .unwrap_or_else(|| panic!("missing [\"stack trace\"=\"...\"] field: {base}"));
    let stack_body = stack_quoted
        .strip_suffix("\"]")
        .unwrap_or_else(|| panic!("stack trace must be the final field: {base}"));
    // `release-checked` builds with `debug = false`, so `Backtrace::force_capture` emits
    // symbolized frames separated by `\n` without `\n\t<file>:<line>` source locations.
    assert!(
        !stack_body.is_empty()
            && stack_body.contains("\\n")
            && (!cfg!(debug_assertions) || stack_body.contains("\\n\\t")),
        "expected non-empty backtrace frames (with \\n\\t under debug): {stack_body:?}"
    );
    format!("{prefix} [\"stack trace\"=\"<STACK>\"]")
}

#[test]
fn runtime_stall_and_panic_logs_match_real_encoder_whole_line() {
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
    let prev_hook = std::panic::take_hook();
    tidb_log::set_panic_hook();

    let rt = Runtime::new(RuntimeConfig {
        worker_threads: 1,
        slice: SliceConfig {
            stall_threshold: Duration::from_millis(80),
            classify_stalls: true,
            ..SliceConfig::default()
        },
        ..RuntimeConfig::default()
    })
    .unwrap();
    let pool = rt.pool().clone();
    let panic_res = rt.block_on(async move {
        pool.spawn("test-log-stall", async {
            std::thread::sleep(Duration::from_millis(140));
        })
        .await
        .unwrap();
        pool.spawn_with_recover(
            "test-log-panic",
            async {
                if std::hint::black_box(true) {
                    panic!("boom for the log");
                }
            },
            |_| {},
        )
        .await
        .unwrap()
    });
    // Restore the test harness panic hook before assertions so any failure prints normally.
    std::panic::set_hook(prev_hook);
    assert_eq!(panic_res, None);

    let out = String::from_utf8(sink.take()).unwrap();
    let lines: Vec<&str> = out.lines().collect();

    // Symbolizing a debug backtrace inside `test-log-panic` on a busy CI VM can itself exceed
    // `stall_threshold`; allow an optional stall warning for `test-log-panic` while still
    // asserting zero unexpected lines.
    let stall_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("[task=test-log-stall]"))
        .collect();
    let panic_lines: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("[\"panic in the recoverable goroutine\"]"))
        .collect();
    for line in &lines {
        assert!(
            line.contains("[task=test-log-stall]")
                || line.contains("[task=test-log-panic]")
                || line.contains("[\"panic in the recoverable goroutine\"]"),
            "unexpected log line:\n{out}"
        );
    }
    assert_eq!(stall_lines.len(), 1, "expected 1 stall log:\n{out}");
    assert_eq!(panic_lines.len(), 1, "expected 1 panic log:\n{out}");

    // 1. Stall warning (tidb-rs-only): verify real encoder quoting and Go duration formatting.
    let stall = normalize_field(&normalize_caller(stall_lines[0]), "elapsed", "<DUR>");
    let stall = normalize_field(&stall, "cause", "<CAUSE>");
    assert_eq!(
        stall,
        "[WARN] [slice.rs:<LINE>] [\"poll stall: a task ran without yielding\"] [cause=<CAUSE>] [task=test-log-stall] [elapsed=<DUR>] [threshold=80ms]"
    );

    // 2. Recoverable goroutine panic (Go pkg/util/misc.go:95 WithRecovery):
    //    exact whole-line match including `["stack trace"="<STACK>"]` with non-empty frames.
    assert_eq!(
        normalize_stack_trace(panic_lines[0]),
        "[ERROR] [pool.rs:<LINE>] [\"panic in the recoverable goroutine\"] [r=\"boom for the log\"] [\"stack trace\"=\"<STACK>\"]"
    );
}
