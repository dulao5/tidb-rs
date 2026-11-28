//! Whole-line golden parity tests for D00.3 Batch 3 structured logs in `tidb-server`.
//! Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;

use tidb_base::config::LogConfig;
use tidb_log::sink::MemorySink;
use tidb_server::column::ResultEncoder;

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

/// One test: the logger and its sink are global to this binary.
#[test]
fn server_logs_match_go_whole_line() {
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

    let mut enc = ResultEncoder::new("utf8mb4");
    enc.update_data_encoding(9999);

    let out = String::from_utf8(sink.take()).unwrap();
    let lines: Vec<String> = out.lines().map(normalize_caller).collect();
    assert_eq!(
        lines.len(),
        2,
        "expected gocharset + result_encoder logs: {lines:?}"
    );
    assert_eq!(
        lines[1],
        "[WARN] [result_encoder.rs:<LINE>] [\"unknown charset ID\"] [error=\"Unknown collation id 9999\"]"
    );

    // `setSystemTimeZoneVariable` (server.go:1137) when the system zone was never set (Go's
    // `GetSystemTZ` sees "System"): the variable keeps its default.
    tidb_server::server::set_system_time_zone_variable();
    let out = String::from_utf8(sink.take()).unwrap();
    let lines: Vec<String> = out.lines().map(normalize_caller).collect();
    assert_eq!(
        lines,
        [
            "[ERROR] [server.rs:<LINE>] [\"Error getting SystemTZ, use default value instead\"] \
          [error=\"variable `systemTZ` is not properly set\"] [\"default system_time_zone\"=CST]"
        ]
    );
    assert_eq!(
        tidb_sessionctx::variable::get_sys_var("system_time_zone")
            .unwrap()
            .value,
        "CST"
    );
}
