//! Whole-line golden parity tests for D00.3 Batch 3 structured logs in `tidb-session`:
//! - `GENERAL_LOG` routed to `GeneralLogger` with all 11 Go v8.5.8 fields in exact order
//! - `ExecutePreparedStmt` `"prepared statement not found"` `[stmtID=...]`
//!
//! Dedicated integration test binary (installs global `MemorySink`s).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tidb_base::Error;
use tidb_base::config::LogConfig;
use tidb_log::sink::MemorySink;
use tidb_session::{Dialect, Session};
use tidb_sessionctx::variable::session::GlobalVarAccessor;
use tidb_sessionctx::variable::tidb_vars::ProcessGeneralLog;

struct Defaults;

impl GlobalVarAccessor for Defaults {
    fn get_global_sys_var(&self, name: &str) -> Result<String, Error> {
        Ok(tidb_sessionctx::variable::get_sys_var(name)
            .map(|v| v.value.clone())
            .unwrap_or_default())
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

#[tokio::test]
async fn general_log_and_prepared_stmt_not_found_match_go_whole_line() {
    let main_sink = MemorySink::new();
    let general_sink = MemorySink::new();
    let cfg = LogConfig {
        level: "debug".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    let main_log = Arc::clone(&main_sink);
    let general_log = Arc::clone(&general_sink);
    tidb_log::init_logger_with_sinks(&cfg, "", main_log, Some(general_log)).unwrap();

    let mut s = Session::new(Dialect::MySQL, Box::new(Defaults));
    s.vars_mut().connection_id = 7;
    s.vars_mut()
        .set_system_var("tidb_session_alias", "s1")
        .unwrap();
    s.vars_mut().current_db = "test".to_owned();

    // 1. GENERAL_LOG routed to GeneralLogger sink with Go's exact 11 fields.
    ProcessGeneralLog.store(true, Ordering::SeqCst);
    let parsed = tidb_parser::parse("SELECT 1", &tidb_parser::ParseOptions::default()).unwrap();
    let stmt = parsed.stmts.into_iter().next().unwrap();
    let res = s.execute_stmt(stmt, "SELECT 1").await;
    ProcessGeneralLog.store(false, Ordering::SeqCst);
    if let Ok(Some(rs)) = res {
        let _ = rs.close(&mut s).await;
    }

    let general_out = String::from_utf8(general_sink.take()).unwrap();
    let general_lines: Vec<String> = general_out.lines().map(normalize_caller).collect();
    assert_eq!(
        general_lines,
        vec![
            "[INFO] [session.rs:<LINE>] [GENERAL_LOG] [conn=7] [session_alias=s1] [user=] [schemaVersion=0] [txnStartTS=0] [forUpdateTS=0] [isReadConsistency=false] [currentDB=test] [isPessimistic=false] [sessionTxnMode=PESSIMISTIC] [sql=\"SELECT 1\"]"
        ]
    );
    let main_after_select = String::from_utf8(main_sink.take()).unwrap();
    assert!(
        !main_after_select.contains("GENERAL_LOG"),
        "GENERAL_LOG must route to GeneralLogger, not Main: {main_after_select}"
    );

    // 1b. GENERAL_LOG with long multiline SQL (> QueryLogMaxLen = 4096) and RC session outside txn:
    //     Go applies QueryReplacer.Replace (no truncation) and vars.IsIsolation(ReadCommitted).
    s.vars_mut()
        .set_system_var("transaction_isolation", "READ-COMMITTED")
        .unwrap();
    let long_lit = "x".repeat(4200);
    let long_sql = format!("SELECT\n\t'{long_lit}'");
    let expected_sql = format!("SELECT  '{long_lit}'");
    ProcessGeneralLog.store(true, Ordering::SeqCst);
    let parsed_long = tidb_parser::parse(&long_sql, &tidb_parser::ParseOptions::default()).unwrap();
    let stmt_long = parsed_long.stmts.into_iter().next().unwrap();
    let res_long = s.execute_stmt(stmt_long, &long_sql).await;
    ProcessGeneralLog.store(false, Ordering::SeqCst);
    if let Ok(Some(rs)) = res_long {
        let _ = rs.close(&mut s).await;
    }
    let general_long_out = String::from_utf8(general_sink.take()).unwrap();
    let general_long_lines: Vec<String> = general_long_out.lines().map(normalize_caller).collect();
    assert_eq!(
        general_long_lines,
        vec![format!(
            "[INFO] [session.rs:<LINE>] [GENERAL_LOG] [conn=7] [session_alias=s1] [user=] [schemaVersion=0] [txnStartTS=0] [forUpdateTS=0] [isReadConsistency=true] [currentDB=test] [isPessimistic=false] [sessionTxnMode=PESSIMISTIC] [sql=\"{expected_sql}\"]"
        )]
    );
    let _ = main_sink.take();

    // 2. ExecutePreparedStmt missing stmtID.
    assert!(s.execute_prepared_stmt(99, vec![]).await.is_err());
    let main_out = String::from_utf8(main_sink.take()).unwrap();
    assert_eq!(
        normalize_caller(main_out.trim_end_matches('\n')),
        "[ERROR] [session.rs:<LINE>] [\"prepared statement not found\"] [stmtID=99]"
    );

    // 3. LazyTxn::go_string matches Go LazyTxn.GoString format.
    assert_eq!(
        tidb_session::txn::LazyTxn::default().go_string(),
        "Txn{state=invalid}"
    );

    // 4. Slow transaction log routes `events` through real `tidb-log` text encoder as
    //    `LogValue::Objects`, matching Go `zap.Objects` (`[{event="txn end",gap=...}]`).
    s.vars_mut().slow_txn_threshold = 1;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let mut tm = tidb_session::txnmanager::new_txn_manager();
    std::thread::sleep(std::time::Duration::from_millis(5));
    tm.on_txn_end(s.sctx());
    let slow_out = String::from_utf8(main_sink.take()).unwrap();
    let slow_line = normalize_caller(slow_out.trim_end_matches('\n'));
    // Normalize the two dynamic durations (`[duration=...]` and `gap=...}`).
    let (head, rest) = slow_line
        .split_once("[duration=")
        .unwrap_or_else(|| panic!("missing duration field: {slow_line}"));
    let (_, after_dur) = rest
        .split_once(']')
        .unwrap_or_else(|| panic!("malformed duration field: {slow_line}"));
    let (before_gap, gap_tail) = after_dur
        .split_once(",gap=")
        .unwrap_or_else(|| panic!("missing gap field in events: {slow_line}"));
    let (gap_val, end_tail) = gap_tail
        .split_once('}')
        .unwrap_or_else(|| panic!("malformed gap field in events: {slow_line}"));
    assert!(
        !gap_val.is_empty() && gap_val.ends_with('s'),
        "expected Go duration_string for gap, got {gap_val:?} in {slow_line}"
    );
    let normalized_slow = format!("{head}[duration=<DUR>]{before_gap},gap=<DUR>}}{end_tail}");
    assert_eq!(
        normalized_slow,
        "[INFO] [txnmanager.rs:<LINE>] [\"slow transaction\"] [duration=<DUR>] [conn=7] [txnStartTS=0] [events=\"[\\\"{event=\\\\\\\"txn end\\\\\\\",gap=<DUR>}\\\"]\"]"
    );
}
