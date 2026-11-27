//! End-to-end golden tests for `command dispatched failed`, `COM_STMT_EXECUTE` argument logging,
//! `tidb_redact_log` (`OFF`/`ON`/`MARKER`), and graceful server drain / shutdown against Go v8.5.8
//! golden captures (`specs/D00.3-logging/spec.md` §4.2, §4.3).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder};
use tidb_base::config::LogConfig;
use tidb_base::errors::{RedactLogDisable, RedactLogEnable, RedactLogMarker};
use tidb_log::sink::{LogSink, MemorySink};
use tidb_mysql_protocol::constants::status;
use tidb_mysql_protocol::{
    ConnectionInfo, Credential, OkInfo, Param, PreparedInfo, Response, Server, ServerConfig,
    SessionFactory, SessionHandler, SqlError,
};
use tokio::net::TcpListener;

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn normalize_line(line: &str) -> String {
    let mut s = line.trim_end_matches('\n').to_owned();
    if s.starts_with('[')
        && let Some(end) = s.find(']')
        && end == 31
    {
        s.replace_range(1..end, "<TS>");
    }
    if let Some(lvl_end) = s.find("] [").and_then(|i| {
        let rest = &s[i + 3..];
        rest.find("] [").map(|j| i + 3 + j + 3)
    }) && let Some(caller_end) = s[lvl_end..].find(']')
    {
        let caller = &s[lvl_end..lvl_end + caller_end];
        if caller.contains(':') && !caller.contains(' ') {
            s.replace_range(lvl_end..lvl_end + caller_end, "<CALLER>");
        }
    }
    // Normalize ephemeral client peer port (`addr:127.0.0.1:<port>` -> `addr:127.0.0.1:<PORT>`).
    if let Some(pos) = s.find("addr:127.0.0.1:") {
        let start = pos + "addr:127.0.0.1:".len();
        let len = s[start..]
            .bytes()
            .take_while(|b| b.is_ascii_digit())
            .count();
        if len > 0 {
            s.replace_range(start..start + len, "<PORT>");
        }
    }
    s
}

#[derive(Clone)]
struct LogTestFactory {
    slow_sleep: Duration,
}

impl SessionFactory for LogTestFactory {
    type Session = LogTestSession;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == "root").then(|| Credential::native_from_password(""))
    }

    fn open_session(&self, _info: &ConnectionInfo) -> Result<LogTestSession, SqlError> {
        Ok(LogTestSession {
            redact: RedactLogDisable.to_owned(),
            status_flags: status::AUTOCOMMIT,
            slow_sleep: self.slow_sleep,
        })
    }
}

struct LogTestSession {
    redact: String,
    status_flags: u16,
    slow_sleep: Duration,
}

impl SessionHandler for LogTestSession {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        if sql == "SET REDACT ON" {
            self.redact = RedactLogEnable.to_owned();
            return Ok(Response::Ok(OkInfo::default()));
        }
        if sql == "SET REDACT MARKER" {
            self.redact = RedactLogMarker.to_owned();
            return Ok(Response::Ok(OkInfo::default()));
        }
        if sql == "SET REDACT OFF" {
            self.redact = RedactLogDisable.to_owned();
            return Ok(Response::Ok(OkInfo::default()));
        }
        if sql == "SLOW" {
            tokio::time::sleep(self.slow_sleep).await;
            return Ok(Response::Ok(OkInfo::default()));
        }
        if sql == "TRIGGER UNDETERMINED" {
            let src =
                tidb_base::terror::ErrResultUndetermined.fast_gen_by_args(tidb_base::go_args![]);
            return Err(SqlError::new(1105, "result undetermined").with_source_err(src));
        }
        if sql == "SELECT * FROM test.no_such_table" {
            let src = tidb_base::terror::ClassSchema
                .new_std(tidb_base::errno::ErrNoSuchTable)
                .gen_with_stack_by_args(tidb_base::go_args!["test", "no_such_table"]);
            return Err(
                SqlError::new(1146, "Table 'test.no_such_table' doesn't exist")
                    .with_source_err(src),
            );
        }
        if sql.contains("FROM nonexistent") || sql.contains("bad syntax here") {
            let near = if sql.contains("bad syntax here") {
                "You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 1 column 55 near \"syntax here\""
            } else {
                "You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 3 column 7 near \"FROM nonexistent\""
            };
            let src = tidb_base::terror::ClassParser
                .new_std(tidb_base::errno::ErrParse)
                .fast_gen_by_args(tidb_base::go_args![near, ""]);
            return Err(SqlError::new(1064, near).with_source_err(src));
        }
        if sql.contains("9223372036854775807 + 1") {
            let src = tidb_base::terror::ClassTypes
                .new_std(tidb_base::errno::ErrDataOutOfRange)
                .gen_with_stack_by_args(tidb_base::go_args!["BIGINT", "(9223372036854775807 + 1)"]);
            return Err(SqlError::new(
                1690,
                "BIGINT value is out of range in '(9223372036854775807 + 1)'",
            )
            .with_source_err(src));
        }
        Ok(Response::Ok(OkInfo::default()))
    }

    async fn prepare(&mut self, _stmt_id: u32, _sql: &str) -> Result<PreparedInfo, SqlError> {
        Ok(PreparedInfo {
            param_count: 2,
            ..PreparedInfo::default()
        })
    }

    async fn execute(&mut self, _stmt_id: u32, _params: Vec<Param>) -> Result<Response, SqlError> {
        let src = tidb_base::terror::ClassSchema
            .new_std(tidb_base::errno::ErrNoSuchTable)
            .gen_with_stack_by_args(tidb_base::go_args!["test", "t"]);
        Err(SqlError::new(1146, "Table 'test.t' doesn't exist").with_source_err(src))
    }

    fn close(&mut self, _stmt_id: u32) {}

    async fn use_db(&mut self, _db: &str) -> Result<(), SqlError> {
        Ok(())
    }

    fn status_flags(&self) -> u16 {
        self.status_flags
    }

    fn redact_mode(&self) -> &str {
        &self.redact
    }

    fn collation_name(&self) -> &str {
        "utf8mb4_0900_ai_ci"
    }
}

#[tokio::test]
async fn golden_command_dispatched_failed_and_graceful_drain() {
    let _g = TEST_LOCK.lock().await;
    let sink = MemorySink::new();
    let lcfg = LogConfig {
        level: "info".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: false,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&lcfg, "", Arc::clone(&sink) as Arc<dyn LogSink>, None)
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let srv_cfg = ServerConfig {
        drain_wait: Duration::from_secs(15),
        cancel_wait: Duration::from_secs(1),
        ..ServerConfig::default()
    };
    let srv = tokio::spawn(async move {
        Server::new(
            srv_cfg,
            LogTestFactory {
                slow_sleep: Duration::from_secs(5),
            },
        )
        .serve(listener, async move {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });

    let opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root")),
    );
    let mut conn = mysql_async::Conn::new(opts.clone()).await.unwrap();
    // Clear startup "server is running MySQL protocol" line.
    let _ = sink.take();

    // Golden Sample 1: Table not exists (1146, no stack).
    let _ = conn.query_drop("SELECT * FROM test.no_such_table").await;
    let l1 = String::from_utf8(sink.take()).unwrap();
    let go_s1 = "[2026/11/27 01:22:37.958 +00:00] [INFO] [conn.go:1222] \
        [\"command dispatched failed\"] [conn=1] [session_alias=] \
        [connInfo=\"id:1, addr:127.0.0.1:52926 status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
        [command=Query] [status=\"inTxn:0, autocommit:1\"] \
        [sql=\"SELECT * FROM test.no_such_table\"] [txn_mode=PESSIMISTIC] [timestamp=0] \
        [err=\"[schema:1146]Table 'test.no_such_table' doesn't exist\"]";
    assert_eq!(normalize_line(&l1), normalize_line(go_s1));

    // Golden Sample 2: Multiline SQL syntax error folded to single spaces.
    let _ = conn.query_drop("SELECT\n  1,\n  FROM nonexistent").await;
    let l2 = String::from_utf8(sink.take()).unwrap();
    let go_s2 = "[2026/11/27 01:22:37.964 +00:00] [INFO] [conn.go:1222] \
        [\"command dispatched failed\"] [conn=1] [session_alias=] \
        [connInfo=\"id:1, addr:127.0.0.1:52928 status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
        [command=Query] [status=\"inTxn:0, autocommit:1\"] \
        [sql=\"SELECT   1,   FROM nonexistent\"] [txn_mode=PESSIMISTIC] [timestamp=0] \
        [err=\"[parser:1064]You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 3 column 7 near \\\"FROM nonexistent\\\" \"]";
    assert_eq!(normalize_line(&l2), normalize_line(go_s2));

    // Golden Sample 3: Runtime overflow error includes stack trace in [err="...\n..."].
    let _ = conn.query_drop("SELECT\n  9223372036854775807 + 1").await;
    let l3 = String::from_utf8(sink.take()).unwrap();
    let norm_l3 = normalize_line(&l3);
    assert!(
        norm_l3.starts_with(
            "[<TS>] [INFO] [<CALLER>] [\"command dispatched failed\"] [conn=1] [session_alias=] \
             [connInfo=\"id:1, addr:127.0.0.1:<PORT> status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
             [command=Query] [status=\"inTxn:0, autocommit:1\"] \
             [sql=\"SELECT   9223372036854775807 + 1\"] [txn_mode=PESSIMISTIC] [timestamp=0] \
             [err=\"[types:1690]BIGINT value is out of range in '(9223372036854775807 + 1)'\\n"
        ),
        "expected exact prefix + stack trace:\n{norm_l3}"
    );
    assert!(
        norm_l3.contains("\\n\\t"),
        "expected Go-style \\n\\t frame location in stack:\n{norm_l3}"
    );

    // Golden Sample 4: tidb_redact_log = ON -> parser.Normalize SQL + fixed parse error message!
    conn.query_drop("SET REDACT ON").await.unwrap();
    let _ = conn
        .query_drop("SELECT * FROM test.t_err WHERE v = 'secret' AND bad syntax here")
        .await;
    let l4 = String::from_utf8(sink.take()).unwrap();
    let go_s4 = "[2026/11/27 01:22:39.047 +00:00] [INFO] [conn.go:1222] \
        [\"command dispatched failed\"] [conn=1] [session_alias=] \
        [connInfo=\"id:1, addr:127.0.0.1:52990 status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
        [command=Query] [status=\"inTxn:0, autocommit:1\"] \
        [sql=\"select * from `test` . `t_err` where `v` = ? and `bad` `syntax` `here`\"] \
        [txn_mode=PESSIMISTIC] [timestamp=0] \
        [err=\"fail to parse SQL, and must redact the whole error when enable log redaction\"]";
    assert_eq!(normalize_line(&l4), normalize_line(go_s4));

    // Redact MARKER wraps ONLY literals in ‹...›, not the whole statement.
    conn.query_drop("SET REDACT MARKER").await.unwrap();
    let _ = conn
        .query_drop("SELECT * FROM test.t_err WHERE v = 'secret' AND bad syntax here")
        .await;
    let l_marker = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_line(&l_marker),
        "[<TS>] [INFO] [<CALLER>] [\"command dispatched failed\"] [conn=1] [session_alias=] \
         [connInfo=\"id:1, addr:127.0.0.1:<PORT> status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
         [command=Query] [status=\"inTxn:0, autocommit:1\"] \
         [sql=\"select * from `test` . `t_err` where `v` = ‹secret› and `bad` `syntax` `here`\"] \
         [txn_mode=PESSIMISTIC] [timestamp=0] \
         [err=\"fail to parse SQL, and must redact the whole error when enable log redaction\"]"
    );

    // COM_STMT_EXECUTE appends ` [arguments: ...]` matching Go DatumsToStringSmart when redact != ON.
    conn.query_drop("SET REDACT OFF").await.unwrap();
    let stmt = conn
        .prep("SELECT * FROM test.t WHERE a = ? AND b = ?")
        .await
        .unwrap();
    let _ = conn.exec_drop(&stmt, (42_i64, "hello")).await;
    let l_exec = String::from_utf8(sink.take()).unwrap();
    assert_eq!(
        normalize_line(&l_exec),
        "[<TS>] [INFO] [<CALLER>] [\"command dispatched failed\"] [conn=1] [session_alias=] \
         [connInfo=\"id:1, addr:127.0.0.1:<PORT> status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
         [command=Execute] [status=\"inTxn:0, autocommit:1\"] \
         [sql=\"SELECT * FROM test.t WHERE a = ? AND b = ? [arguments: (42, \\\"hello\\\")]\"] \
         [txn_mode=PESSIMISTIC] [timestamp=0] \
         [err=\"[schema:1146]Table 'test.t' doesn't exist\"]"
    );

    // NULL, non-printable hex (0x...), and >2048 truncation with ` len(N)` match Go DatumsToStringSmart.
    let long_s = "a".repeat(2050);
    let fmt_args = tidb_mysql_protocol::format_stmt_execute_sql_for_log(
        "SELECT ?, ?, ?",
        &[
            Param {
                ty: tidb_mysql_protocol::ColumnType::Null,
                unsigned: false,
                value: tidb_mysql_protocol::Value::Null,
            },
            Param {
                ty: tidb_mysql_protocol::ColumnType::VarString,
                unsigned: false,
                value: tidb_mysql_protocol::Value::Bytes(vec![0x00, 0x01, 0xff]),
            },
            Param {
                ty: tidb_mysql_protocol::ColumnType::VarString,
                unsigned: false,
                value: tidb_mysql_protocol::Value::Bytes(long_s.into_bytes()),
            },
        ],
        RedactLogDisable,
    );
    let expected_trunc = format!(
        "SELECT ?, ?, ? [arguments: (NULL, 0x0001FF, \"{}\" len(2050))]",
        "a".repeat(2048)
    );
    assert_eq!(fmt_args, expected_trunc);

    // ErrResultUndetermined (Go conn.go:1194-1197): logs WARN and closes the connection directly
    // WITHOUT writing a MySQL error packet to the client.
    let mut conn2 = mysql_async::Conn::new(opts).await.unwrap();
    let err2 = conn2
        .query_drop("TRIGGER UNDETERMINED")
        .await
        .expect_err("connection must close on ErrResultUndetermined");
    assert!(
        !matches!(err2, mysql_async::Error::Server(_)),
        "ErrResultUndetermined must close connection without sending a MySQL ERR packet, got: {err2:?}"
    );
    let l_undet = String::from_utf8(sink.take()).unwrap();
    assert!(
        l_undet.contains("[\"result undetermined, close this connection\"]"),
        "expected result undetermined WARN log, got: {l_undet}"
    );
    assert!(
        !l_undet.contains("command dispatched failed"),
        "ErrResultUndetermined must not emit command dispatched failed: {l_undet}"
    );

    // Graceful shutdown with ONE IDLE pooled client still open:
    // Go skips idle connections outside a transaction and IMMEDIATELY logs
    // `all sessions quit in drain wait time` + `kill all connections.` (zero 5s/15s stall!).
    let t0 = Instant::now();
    let _ = stop_tx.send(());
    srv.await.unwrap();
    let drain_elapsed = t0.elapsed();
    assert!(
        drain_elapsed < Duration::from_secs(2),
        "idle connection must NOT stall graceful drain (took {drain_elapsed:?})"
    );
    let shutdown_log = String::from_utf8(sink.take()).unwrap();
    let shutdown_lines: Vec<String> = shutdown_log.lines().map(normalize_line).collect();
    assert_eq!(
        shutdown_lines,
        vec![
            "[<TS>] [INFO] [<CALLER>] [\"start drain clients\"]",
            "[<TS>] [INFO] [<CALLER>] [\"all sessions quit in drain wait time\"]",
            "[<TS>] [INFO] [<CALLER>] [\"kill all connections.\"] [category=server]",
        ]
    );
    drop(conn);
}
