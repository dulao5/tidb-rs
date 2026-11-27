//! Expensive query monitoring and OOM memory-exceeded log formatting.
//!
//! go-file: pkg/util/expensivequery/expensivequery.go
//! go-file: pkg/util/util.go
//!
//! Aligns byte-for-byte with Go TiDB v8.5.8 `util.GenLogFields` (14-field order),
//! `expensive_query`, and `memory exceeds quota` (`specs/D00.3-logging/spec.md` §4.4).

use std::time::Duration;

use tidb_base::gostd::strconv::format_float;
use tidb_base::structured_log::{LogField, LogLevel, LogRecord, LogTarget};
use tidb_execdetails::execdetails::{
    CopTasksDetails, ExecDetails, LogField as ExecLogField, LogValue as ExecLogValue,
};
use tidb_log::{OwnedLogField, OwnedLogValue};
use tidb_memory::tracker::format_bytes;

/// Default SQL truncation length for `util.GenLogFields` (`bigIntMaxLen = 8192`).
pub const EXPENSIVE_QUERY_SQL_MAX_LEN: usize = 8192;

/// Snapshot of process/statement metadata consumed by [`gen_log_fields`].
#[derive(Clone, Debug, Default)]
pub struct ProcessInfoForLog {
    pub conn_id: u64,
    pub user: String,
    pub db: String,
    /// Table IDs accessed by the statement (Go preserves duplicates, e.g. `[100,100,200]`).
    pub table_ids: Vec<i64>,
    pub index_names: Vec<String>,
    pub txn_start_ts: u64,
    /// Maximum memory bytes tracked (`None` omits `mem_max`).
    pub mem_max_bytes: Option<i64>,
    /// Pre-formatted stats summary (e.g. `"big:pseudo"` or `"t:123"`).
    pub stats_info: String,
    /// SQL text (normalized / redacted by caller; raw `\n` is preserved per Go `GenLogFields`).
    pub sql: String,
    pub session_alias: String,
    pub affected_rows: u64,
    pub exec_details: Option<ExecDetails>,
    pub cop_tasks: Option<CopTasksDetails>,
}

/// Build the exact 14-section field list of Go `util.GenLogFields(costTime, info, true)`.
///
/// Field order (optional sections omitted when empty/zero):
/// 1. `cost_time` (always: `strconv.FormatFloat(costTime.Seconds(), 'f', -1, 64) + "s"`)
/// 2. `ExecDetails::to_zap_fields` (`cop_time`, `process_time`, `wait_time`, `backoff_time`,
///    stringified `request_count`, `total_keys`, `process_keys`, ...)
/// 3. `CopTasksDetails::to_zap_fields` (`num_cop_tasks`, `process_{avg,p90,max}_time`,
///    `process_max_addr`, `wait_{avg,p90,max}_time`, `wait_max_addr`)
/// 4. `stats`
/// 5. `conn` (when `conn_id != 0`)
/// 6. `user`
/// 7. `database`
/// 8. `table_ids` (preserves duplicates, e.g. `"[100,100,200]"`)
/// 9. `index_names`
/// 10. `txn_start_ts` (always)
/// 11. `mem_max` (when `Some`, `"<N> Bytes (<memory.FormatBytes(N)>)"`)
/// 12. `sql` (preserves `\n`, truncates at `max_sql_len` with `" len(<total>)"`)
/// 13. `session_alias` (always)
/// 14. `"affected rows"` (always)
// go: pkg/util/util.go GenLogFields L2 (takes ProcessInfoForLog snapshot + tidb_execdetails::ExecDetails)
pub fn gen_log_fields(
    cost_time: Duration,
    info: &ProcessInfoForLog,
    max_sql_len: usize,
) -> Vec<OwnedLogField> {
    let mut fields = Vec::with_capacity(24);

    // 1. cost_time: Go pkg/util/util.go:108 `strconv.FormatFloat(costTime.Seconds(), 'f', -1, 64) + "s"`.
    let cost_s = format!("{}s", format_float(cost_time.as_secs_f64(), b'f', -1, 64));
    fields.push(OwnedLogField::string("cost_time", cost_s));

    // 2. ExecDetails::to_zap_fields (keeps request_count/total_keys/process_keys as strings)
    if let Some(detail) = &info.exec_details {
        for f in detail.to_zap_fields() {
            fields.push(convert_exec_field(f));
        }
    }

    // 3. CopTasksDetails::to_zap_fields
    if let Some(cop) = &info.cop_tasks
        && cop.num_cop_tasks > 0
    {
        for f in CopTasksDetails::to_zap_fields(Some(cop)) {
            fields.push(convert_exec_field(f));
        }
    }

    // 4. stats
    if !info.stats_info.is_empty() {
        fields.push(OwnedLogField::string("stats", &info.stats_info));
    }

    // 5. conn
    if info.conn_id != 0 {
        fields.push(OwnedLogField::u64("conn", info.conn_id));
    }

    // 6. user
    if !info.user.is_empty() {
        fields.push(OwnedLogField::string("user", &info.user));
    }

    // 7. database
    if !info.db.is_empty() {
        fields.push(OwnedLogField::string("database", &info.db));
    }

    // 8. table_ids (Go preserves duplicates, formatted as "[id1,id2,...]")
    if !info.table_ids.is_empty() {
        let mut s = String::from("[");
        for (i, id) in info.table_ids.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&id.to_string());
        }
        s.push(']');
        fields.push(OwnedLogField::string("table_ids", s));
    }

    // 9. index_names
    if !info.index_names.is_empty() {
        let mut s = String::from("[");
        for (i, idx) in info.index_names.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(idx);
        }
        s.push(']');
        fields.push(OwnedLogField::string("index_names", s));
    }

    // 10. txn_start_ts
    fields.push(OwnedLogField::u64("txn_start_ts", info.txn_start_ts));

    // 11. mem_max: Go pkg/util/util.go:152 `fmt.Sprintf("%v Bytes (%v)", memMax, memory.FormatBytes(memMax))`
    if let Some(mem) = info.mem_max_bytes {
        let formatted = format!("{mem} Bytes ({})", format_bytes(mem));
        fields.push(OwnedLogField::string("mem_max", formatted));
    }

    // 12. sql (preserves raw \n; truncates at max_sql_len with " len(<total>)")
    if !info.sql.is_empty() {
        let sql_str = truncate_expensive_sql(&info.sql, max_sql_len);
        fields.push(OwnedLogField::string("sql", sql_str));
    }

    // 13. session_alias
    fields.push(OwnedLogField::string("session_alias", &info.session_alias));

    // 14. "affected rows"
    fields.push(OwnedLogField::u64("affected rows", info.affected_rows));

    fields
}

fn convert_exec_field(f: ExecLogField) -> OwnedLogField {
    let value = match f.value {
        ExecLogValue::String(s) => OwnedLogValue::Str(s),
        ExecLogValue::Int(n) => OwnedLogValue::I64(n),
    };
    OwnedLogField { key: f.key, value }
}

fn truncate_expensive_sql(sql: &str, max_len: usize) -> String {
    if max_len == 0 || sql.len() <= max_len {
        return sql.to_owned();
    }
    let mut end = max_len;
    while end > 0 && !sql.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} len({})", &sql[..end], sql.len())
}

/// Emit `[WARN] [expensive_query]` with [`gen_log_fields`], gated on `logger().level() <= Warn`
/// before constructing fields (Go `expensivequery.go:92-95`).
// go: pkg/util/expensivequery/expensivequery.go logExpensiveQuery L1
pub fn log_expensive_query(cost_time: Duration, info: &ProcessInfoForLog) {
    let logger = tidb_base::structured_log::logger();
    if logger.level() > LogLevel::Warn {
        return;
    }
    let owned = gen_log_fields(cost_time, info, EXPENSIVE_QUERY_SQL_MAX_LEN);
    let borrowed: Vec<LogField<'_>> = owned.iter().map(OwnedLogField::as_field).collect();
    logger.log(&LogRecord {
        target: LogTarget::Main,
        level: LogLevel::Warn,
        logger_name: "",
        file: file!(),
        line: line!(),
        msg: "expensive_query",
        fields: &borrowed,
    });
}

/// Emit `[WARN] ["memory exceeds quota"]` with [`gen_log_fields`] (or `[INFO]`
/// `"expensive_query during bootstrap phase"` `[conn=...]` when `info` is `None`), gated on
/// `logger().level() <= Warn` (Go `expensivequery.go:126-154`).
// go: pkg/util/expensivequery/expensivequery.go Handle.LogOnQueryExceedMemQuota L1
pub fn log_on_query_exceed_mem_quota(
    conn_id: u64,
    cost_time: Duration,
    info: Option<&ProcessInfoForLog>,
) {
    let logger = tidb_base::structured_log::logger();
    if logger.level() > LogLevel::Warn {
        return;
    }
    let Some(info) = info else {
        tidb_base::log_info!("expensive_query during bootstrap phase", "conn" => conn_id);
        return;
    };
    let owned = gen_log_fields(cost_time, info, EXPENSIVE_QUERY_SQL_MAX_LEN);
    let borrowed: Vec<LogField<'_>> = owned.iter().map(OwnedLogField::as_field).collect();
    logger.log(&LogRecord {
        target: LogTarget::Main,
        level: LogLevel::Warn,
        logger_name: "",
        file: file!(),
        line: line!(),
        msg: "memory exceeds quota",
        fields: &borrowed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_execdetails::clientgo::ScanDetail;
    use tidb_execdetails::execdetails::{CopTasksDetails, ExecDetails};

    #[test]
    fn gen_log_fields_matches_go_14_field_order_and_json_types() {
        let detail = ExecDetails {
            cop_time: 204_207, // 0.000204207s
            request_count: 1,
            scan_detail: Some(Box::new(ScanDetail {
                total_keys: 10,
                processed_keys: 8,
                ..ScanDetail::default()
            })),
            ..ExecDetails::default()
        };
        let cop = CopTasksDetails {
            num_cop_tasks: 1,
            max_process_address: "store1".to_owned(),
            max_wait_address: "store1".to_owned(),
            ..CopTasksDetails::default()
        };
        let info = ProcessInfoForLog {
            conn_id: 2097176,
            user: "root".to_owned(),
            db: "test".to_owned(),
            table_ids: vec![100, 100, 200], // duplicates preserved per Go
            index_names: vec!["idx_k".to_owned()],
            txn_start_ts: 470743179645681664,
            mem_max_bytes: Some(2146),
            stats_info: "big:pseudo".to_owned(),
            sql: "SELECT\n  1\nFROM t".to_owned(),
            session_alias: String::new(),
            affected_rows: 0,
            exec_details: Some(detail),
            cop_tasks: Some(cop),
        };
        let fields = gen_log_fields(Duration::from_nanos(10_043_440_202), &info, 8192);
        let keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
        // Verify exact section ordering: cost_time -> execdetails -> cop_tasks -> stats -> conn
        // -> user -> database -> table_ids -> index_names -> txn_start_ts -> mem_max -> sql
        // -> session_alias -> "affected rows".
        assert_eq!(keys[0], "cost_time");
        assert!(keys.contains(&"cop_time"));
        assert!(keys.contains(&"request_count"));
        assert!(keys.contains(&"num_cop_tasks"));
        let tail_start = keys.iter().position(|k| *k == "stats").unwrap();
        assert_eq!(
            &keys[tail_start..],
            &[
                "stats",
                "conn",
                "user",
                "database",
                "table_ids",
                "index_names",
                "txn_start_ts",
                "mem_max",
                "sql",
                "session_alias",
                "affected rows",
            ]
        );
        // Verify stringified request_count/total_keys/process_keys (Go JSON string parity).
        let req_cnt = fields.iter().find(|f| f.key == "request_count").unwrap();
        assert_eq!(req_cnt.value, OwnedLogValue::Str("1".to_owned()));
        // Verify duplicate table_ids preserved.
        let tids = fields.iter().find(|f| f.key == "table_ids").unwrap();
        assert_eq!(tids.value, OwnedLogValue::Str("[100,100,200]".to_owned()));
        // Verify mem_max human formatting and raw \n preserved in sql.
        let mem = fields.iter().find(|f| f.key == "mem_max").unwrap();
        assert_eq!(
            mem.value,
            OwnedLogValue::Str("2146 Bytes (2.10 KB)".to_owned())
        );
        let sql_f = fields.iter().find(|f| f.key == "sql").unwrap();
        assert_eq!(
            sql_f.value,
            OwnedLogValue::Str("SELECT\n  1\nFROM t".to_owned())
        );

        // Verify cost_time uses seconds float (not µs/ms) matching Go pkg/util/util.go:108.
        let sub_ms = gen_log_fields(Duration::from_nanos(197_248), &info, 8192);
        assert_eq!(
            sub_ms[0].value,
            OwnedLogValue::Str("0.000197248s".to_owned())
        );
        // Verify 1024 boundary uses memory::format_bytes (> 1024 -> KB, <= 1024 -> Bytes).
        let exact_kb = ProcessInfoForLog {
            mem_max_bytes: Some(1024),
            ..ProcessInfoForLog::default()
        };
        let kb_fields = gen_log_fields(Duration::from_millis(1), &exact_kb, 8192);
        let kb_mem = kb_fields.iter().find(|f| f.key == "mem_max").unwrap();
        assert_eq!(
            kb_mem.value,
            OwnedLogValue::Str("1024 Bytes (1024 Bytes)".to_owned())
        );

        // Verify SQL truncation at max_sql_len with " len(<total>)".
        let long_info = ProcessInfoForLog {
            sql: "SELECT 1234567890".to_owned(),
            ..ProcessInfoForLog::default()
        };
        let trunc = gen_log_fields(Duration::from_millis(1), &long_info, 10);
        let trunc_sql = trunc.iter().find(|f| f.key == "sql").unwrap();
        assert_eq!(
            trunc_sql.value,
            OwnedLogValue::Str("SELECT 123 len(17)".to_owned())
        );
    }

    #[test]
    fn log_expensive_query_and_oom_respect_level_gate_and_format() {
        let sink = tidb_log::MemorySink::new();
        let cfg = tidb_base::config::LogConfig {
            level: "warn".to_owned(),
            format: "text".to_owned(),
            disable_timestamp: false,
            disable_error_verbose: true,
            ..tidb_base::config::LogConfig::default()
        };
        tidb_log::init_logger_with_sinks(
            &cfg,
            "",
            std::sync::Arc::clone(&sink) as std::sync::Arc<dyn tidb_log::LogSink>,
            None,
        )
        .unwrap();

        let info = ProcessInfoForLog {
            conn_id: 2097198,
            user: "root".to_owned(),
            txn_start_ts: 0,
            mem_max_bytes: Some(2032),
            sql: "SELECT\n  1".to_owned(),
            session_alias: String::new(),
            affected_rows: 0,
            ..ProcessInfoForLog::default()
        };
        log_expensive_query(Duration::from_nanos(197_248), &info);
        let line1 = String::from_utf8(sink.take()).unwrap();
        assert!(line1.contains("[WARN]"), "{line1}");
        assert!(
            line1.contains("[expensive_query] [cost_time=0.000197248s]"),
            "{line1}"
        );
        assert!(
            line1.contains(r#"[mem_max="2032 Bytes (1.98 KB)"]"#),
            "{line1}"
        );
        assert!(
            line1.contains(r#"[sql="SELECT\n  1"] [session_alias=] ["affected rows"=0]"#),
            "{line1}"
        );

        log_on_query_exceed_mem_quota(2097198, Duration::from_nanos(197_248), Some(&info));
        let line2 = String::from_utf8(sink.take()).unwrap();
        assert!(
            line2.contains(r#"["memory exceeds quota"] [cost_time=0.000197248s]"#),
            "{line2}"
        );

        // Level > Warn suppresses both without building fields.
        tidb_log::set_level("error").unwrap();
        log_expensive_query(Duration::from_secs(10), &info);
        log_on_query_exceed_mem_quota(2097198, Duration::from_secs(10), Some(&info));
        assert!(sink.snapshot().is_empty());
        tidb_log::set_level("info").unwrap();
    }
}
