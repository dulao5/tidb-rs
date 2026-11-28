//! Whole-line golden log ordering test for `KILL` dispatch logs over the MySQL protocol
//! (`[kill]` -> `"kill initiated"` -> `"kill finished"` -> `"command dispatched failed"`) across
//! all five runtime termination codes (`1317`, `3024`, `8175`, `8176`, `8253`).
//! Dedicated integration test binary (installs global `MemorySink`).

use std::sync::Arc;

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder};
use tidb_base::config::LogConfig;
use tidb_log::sink::{LogSink, MemorySink};
use tidb_memory::sqlkiller::{
    KillSignal, MaxExecTimeExceeded, QueryInterrupted, QueryMemoryExceeded, RunawayQueryExceeded,
    ServerMemoryExceeded,
};
use tidb_mysql_protocol::{
    ColumnDef, ColumnType, ConnectionInfo, Credential, OkInfo, Param, PreparedInfo, Response,
    ResultSet, Server, ServerConfig, SessionFactory, SessionHandler, SqlError, Value,
};
use tidb_server::session_mgr::{ConnControl, ServerSessionManager};
use tidb_sessionctx::session_mgr::SessionManager;
use tokio::net::TcpListener;

#[derive(Clone)]
struct KillLogFactory {
    mgr: Arc<ServerSessionManager>,
    query_entered: Arc<tokio::sync::Notify>,
}

struct KillLogSession {
    ctrl: Arc<ConnControl>,
    query_entered: Arc<tokio::sync::Notify>,
}

impl SessionFactory for KillLogFactory {
    type Session = KillLogSession;

    fn session_manager(&self) -> Option<Arc<ServerSessionManager>> {
        Some(Arc::clone(&self.mgr))
    }

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == "root").then(|| Credential::native_from_password(""))
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<KillLogSession, SqlError> {
        Ok(KillLogSession {
            ctrl: ConnControl::new(info.connection_id),
            query_entered: Arc::clone(&self.query_entered),
        })
    }
}

impl SessionHandler for KillLogSession {
    fn conn_control(&self) -> Option<Arc<ConnControl>> {
        Some(Arc::clone(&self.ctrl))
    }

    fn should_install_connection_alive_during_execute(&self) -> bool {
        false
    }

    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        if sql.eq_ignore_ascii_case("SELECT @@max_allowed_packet,@@wait_timeout") {
            return Ok(Response::ResultSet(ResultSet {
                columns: vec![
                    ColumnDef::new("@@max_allowed_packet", ColumnType::LongLong),
                    ColumnDef::new("@@wait_timeout", ColumnType::LongLong),
                ],
                rows: vec![vec![Value::Int(64 << 20), Value::Int(28800)]],
                warnings: 0,
            }));
        }
        if sql.eq_ignore_ascii_case("SELECT @@max_allowed_packet,@@wait_timeout,@@socket") {
            return Ok(Response::ResultSet(ResultSet {
                columns: vec![
                    ColumnDef::new("@@max_allowed_packet", ColumnType::LongLong),
                    ColumnDef::new("@@wait_timeout", ColumnType::LongLong),
                    ColumnDef::new("@@socket", ColumnType::VarString),
                ],
                rows: vec![vec![Value::Int(64 << 20), Value::Int(28800), Value::Null]],
                warnings: 0,
            }));
        }
        if sql != "SELECT SLOW" {
            return Ok(Response::Ok(OkInfo::default()));
        }
        self.query_entered.notify_one();
        self.ctrl.kill_cancel.cancelled().await;
        match self.ctrl.killer.handle_signal() {
            Err(err) => Err(SqlError::from_tidb_error(&err)),
            Ok(()) => Ok(Response::Ok(OkInfo::default())),
        }
    }

    async fn prepare(&mut self, _: u32, _: &str) -> Result<PreparedInfo, SqlError> {
        Ok(PreparedInfo::default())
    }

    async fn execute(&mut self, _: u32, _: Vec<Param>) -> Result<Response, SqlError> {
        Ok(Response::Ok(OkInfo::default()))
    }

    fn close(&mut self, _: u32) {}

    async fn use_db(&mut self, _: &str) -> Result<(), SqlError> {
        Ok(())
    }

    fn collation_name(&self) -> &str {
        "utf8mb4_0900_ai_ci"
    }
}

fn normalize_line(line: &str) -> String {
    let mut s = line.trim_end_matches('\n').to_owned();
    if let Some((head, tail)) = s.split_once("] [")
        && let Some((caller, rest)) = tail.split_once("] ")
        && let Some((file, digits)) = caller.rsplit_once(':')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        s = format!("{head}] [{file}:<LINE>] {rest}");
    }
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
    if let Some(pos) = s.find("\\n")
        && let Some(end) = s[pos..].find("\"]")
    {
        s.replace_range(pos..pos + end, "<STACK>");
    }
    s
}

#[tokio::test]
async fn dispatch_kill_logs_match_go_sequence_across_all_signals() {
    let sink = MemorySink::new();
    let lcfg = LogConfig {
        level: "info".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&lcfg, "", Arc::clone(&sink) as Arc<dyn LogSink>, None)
        .unwrap();

    let mgr = ServerSessionManager::new_standalone();
    let query_entered = Arc::new(tokio::sync::Notify::new());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let mgr_srv = Arc::clone(&mgr);
    let entered_srv = Arc::clone(&query_entered);
    let srv = tokio::spawn(async move {
        Server::new(
            ServerConfig::default(),
            KillLogFactory {
                mgr: mgr_srv,
                query_entered: entered_srv,
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
            .user(Some("root"))
            .prefer_socket(false),
    );
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    let conn_id = u64::from(conn.id());
    assert_eq!(conn_id, 2_097_154);
    let _ = sink.take();

    let cases: &[(KillSignal, bool, bool, bool, u16, &str)] = &[
        (
            QueryInterrupted,
            true,
            false,
            false,
            1317,
            "[executor:1317]Query execution was interrupted",
        ),
        (
            MaxExecTimeExceeded,
            true,
            true,
            false,
            3024,
            "[executor:3024]Query execution was interrupted, maximum statement execution time exceeded",
        ),
        (
            QueryMemoryExceeded,
            false,
            false,
            false,
            8175,
            "[executor:8175]Your query has been cancelled due to exceeding the allowed memory limit for a single SQL query. Please try narrowing your query scope or increase the tidb_mem_quota_query limit and try again.[conn=2097154]",
        ),
        (
            ServerMemoryExceeded,
            false,
            false,
            false,
            8176,
            "[executor:8176]Your query has been cancelled due to exceeding the allowed memory limit for the tidb-server instance and this query is currently using the most memory. Please try narrowing your query scope or increase the tidb_server_memory_limit and try again.[conn=2097154]",
        ),
        (
            RunawayQueryExceeded,
            true,
            false,
            true,
            8253,
            "[executor:8253]Query execution was interrupted, identified as runaway query [runaway exceed tidb side]",
        ),
    ];

    for &(sig, is_mgr_call, max_exec, runaway, code, err_prefix) in cases {
        let mgr_task = Arc::clone(&mgr);
        let ctrl = mgr.get_conn(conn_id).expect("registered conn");
        let entered = Arc::clone(&query_entered);
        let trigger = tokio::spawn(async move {
            entered.notified().await;
            if is_mgr_call {
                mgr_task.kill(conn_id, true, max_exec, runaway);
            } else {
                ctrl.killer.send_kill_signal(sig);
            }
        });
        let err = conn.query_drop("SELECT SLOW").await.unwrap_err();
        trigger.await.unwrap();
        match err {
            mysql_async::Error::Server(se) => assert_eq!(se.code, code),
            other => panic!("expected error {code}, got {other:?}"),
        }

        let raw = String::from_utf8(sink.take()).unwrap();
        let lines: Vec<String> = raw
            .lines()
            .filter(|l| !l.contains("global memory controller, NeedKill"))
            .map(normalize_line)
            .collect();
        let expected_dispatch = format!(
            "[INFO] [conn.rs:<LINE>] [\"command dispatched failed\"] [conn={conn_id}] [session_alias=] \
             [connInfo=\"id:{conn_id}, addr:127.0.0.1:<PORT> status:10, collation:utf8mb4_0900_ai_ci, user:root\"] \
             [command=Query] [status=\"inTxn:0, autocommit:1\"] [sql=\"SELECT SLOW\"] \
             [txn_mode=PESSIMISTIC] [timestamp=0] [err=\"{err_prefix}{}\"]",
            if sig == RunawayQueryExceeded {
                ""
            } else {
                "<STACK>"
            }
        );
        let mut expected = Vec::new();
        if is_mgr_call {
            expected.push(format!(
                "[INFO] [session_mgr.rs:<LINE>] [kill] [conn={conn_id}] [query=true] [maxExecutionTime={max_exec}] [runawayExceed={runaway}]"
            ));
        }
        expected.push(format!(
            "[WARN] [sqlkiller.rs:<LINE>] [\"kill initiated\"] [\"connection ID\"={conn_id}] [reason=\"{err_prefix}\"]"
        ));
        expected.push(format!(
            "[WARN] [sqlkiller.rs:<LINE>] [\"kill finished\"] [conn={conn_id}]"
        ));
        expected.push(expected_dispatch);
        assert_eq!(lines, expected, "signal {sig} ({code})");
    }

    drop(conn);
    let _ = stop_tx.send(());
    srv.await.unwrap();
}
