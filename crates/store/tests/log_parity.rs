//! Whole-line golden parity tests for D00.3 Batch 2 structured logs in `tidb-store` and
//! `LogValue::I64s` encoding. Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;

use tidb_base::config::LogConfig;
use tidb_base::errors;
use tidb_log::sink::MemorySink;
use tidb_store::driver::txn::error::gen_key_exists_error;

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

#[test]
fn store_structured_logs_and_i64s_match_go_whole_line() {
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

    // 1. tidb-store::driver::txn::error::gen_key_exists_error (Go pkg/store/driver/txn/error.go:46)
    let cause = errors::new("decode key failed");
    let _ = gen_key_exists_error("t.idx", b"v1", Some(&cause));
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[INFO] [error.rs:<LINE>] [\"extractKeyExistsErr meets error\"] [error=\"decode key failed\"]"
    );

    // 2. LogValue::I64s (used by Domain.loadInfoSchema `phyTblIDs`, Go zap.Int64s)
    let ids: &[i64] = &[-1, 110, 111];
    tidb_base::log_info!("ids check", "phyTblIDs" => ids);
    let line = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_caller(line.trim_end_matches('\n')),
        "[INFO] [log_parity.rs:<LINE>] [\"ids check\"] [phyTblIDs=\"[-1,110,111]\"]"
    );
}
