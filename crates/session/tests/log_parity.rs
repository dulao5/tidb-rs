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

    // 2. ExecutePreparedStmt missing stmtID.
    assert!(s.execute_prepared_stmt(99, vec![]).await.is_err());
    let main_out = String::from_utf8(main_sink.take()).unwrap();
    assert_eq!(
        normalize_caller(main_out.trim_end_matches('\n')),
        "[ERROR] [session.rs:<LINE>] [\"prepared statement not found\"] [stmtID=99]"
    );

    // 3. Slow-transaction Event::marshal_log_object uses Go duration_string format.
    let ev = tidb_session::txnmanager::Event {
        event: "enter txn".to_owned(),
        duration: std::time::Duration::from_millis(12),
    };
    assert_eq!(
        ev.marshal_log_object(),
        r#"{"event":"enter txn","gap":"12ms"}"#
    );
}
