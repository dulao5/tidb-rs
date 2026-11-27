//! Whole-line golden parity tests for MySQL protocol connection lifecycle, handshake EOF,
//! authentication / use-db failure, and TLS handshake logs (`specs/D00.3-logging/spec.md` §4.5,
//! Batch 4).
//!
//! Dedicated integration test binary (installs global `MemorySink` at `debug` level).

use std::sync::Arc;
use std::time::Duration;

use mysql_async::{Opts, OptsBuilder};
use tidb_base::config::LogConfig;
use tidb_log::sink::{LogSink, MemorySink};
use tidb_mysql_protocol::{
    ConnectionInfo, Credential, OkInfo, Param, PreparedInfo, Response, Server, ServerConfig,
    SessionFactory, SessionHandler, SqlError,
};
use tokio::io::AsyncReadExt as _;
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct AuthTestFactory;

impl SessionFactory for AuthTestFactory {
    type Session = AuthTestSession;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == "root").then(|| Credential::native_from_password(""))
    }

    fn open_session(&self, _info: &ConnectionInfo) -> Result<AuthTestSession, SqlError> {
        Ok(AuthTestSession)
    }
}

struct AuthTestSession;

impl SessionHandler for AuthTestSession {
    async fn query(&mut self, _sql: &str) -> Result<Response, SqlError> {
        Ok(Response::Ok(OkInfo::default()))
    }
    async fn prepare(&mut self, _stmt_id: u32, _sql: &str) -> Result<PreparedInfo, SqlError> {
        Ok(PreparedInfo::default())
    }
    async fn execute(&mut self, _stmt_id: u32, _params: Vec<Param>) -> Result<Response, SqlError> {
        Ok(Response::Ok(OkInfo::default()))
    }
    fn close(&mut self, _stmt_id: u32) {}
    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        if db == "test" {
            Ok(())
        } else {
            Err(SqlError::bad_db(db))
        }
    }
}

/// Normalize only caller line numbers (`[file.rs:123]` -> `[file.rs:LINE]`) and ephemeral
/// loopback client ports (`127.0.0.1:54321` -> `127.0.0.1:PORT`) so assertions pin whole lines
/// in exact order with no extra or missing lines.
fn normalize_lines(raw: &str) -> Vec<String> {
    raw.lines()
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if line[i..].starts_with(".rs:") {
                    out.push_str(".rs:LINE");
                    i += 4;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                } else if line[i..].starts_with("127.0.0.1:") {
                    out.push_str("127.0.0.1:PORT");
                    i += "127.0.0.1:".len();
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    let ch = line[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            out
        })
        .collect()
}

/// Wait until the sink has produced at least `expected_lines` lines (bounded at 2s) and drain it.
async fn take_lines(sink: &MemorySink, expected_lines: usize) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut acc = Vec::new();
    loop {
        let chunk = String::from_utf8(sink.take()).unwrap();
        if !chunk.is_empty() {
            acc.extend(normalize_lines(&chunk));
        }
        if acc.len() >= expected_lines || tokio::time::Instant::now() >= deadline {
            // Brief settle to catch any unexpected extra line that would violate whole-line parity.
            tokio::time::sleep(Duration::from_millis(30)).await;
            let tail = String::from_utf8(sink.take()).unwrap();
            if !tail.is_empty() {
                acc.extend(normalize_lines(&tail));
            }
            return acc;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn handshake_eof_auth_failure_and_lifecycle_logs_match_go() {
    let sink = MemorySink::new();
    let cfg = LogConfig {
        level: "debug".to_owned(),
        format: "text".to_owned(),
        disable_timestamp: true,
        disable_error_verbose: true,
        ..LogConfig::default()
    };
    tidb_log::init_logger_with_sinks(&cfg, "", Arc::clone(&sink) as Arc<dyn LogSink>, None)
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), AuthTestFactory)
            .serve(listener, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });

    // Startup line.
    let startup = take_lines(&sink, 1).await;
    assert_eq!(
        startup,
        vec![
            "[INFO] [server.rs:LINE] [\"server is running MySQL protocol\"] [addr=127.0.0.1:PORT]"
        ]
    );

    // 1. Raw TCP connect, read initial handshake greeting, then close (EOF).
    let mut raw = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 256];
    let _ = raw.read(&mut buf).await.unwrap();
    drop(raw);

    let eof_lines = take_lines(&sink, 3).await;
    assert_eq!(
        eof_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[DEBUG] [conn.rs:LINE] [\"wait handshake response fail due to connection has be closed by client-side\"] [conn=1]",
            "[DEBUG] [conn.rs:LINE] [EOF] [conn=1] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 2. Authentication failure (unknown user): both conn.go:677 WARN and server.go:712 WARN
    //    must be emitted with `[privilege:1045]` RFC prefix.
    let bad_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("nobody")),
    );
    assert!(mysql_async::Conn::new(bad_opts).await.is_err());

    let auth_lines = take_lines(&sink, 3).await;
    assert_eq!(
        auth_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[WARN] [conn.rs:LINE] [\"open new session or authentication failure\"] [conn=2] [error=\"[privilege:1045]Access denied for user 'nobody'@'127.0.0.1' (using password: NO)\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=2] [error=\"[privilege:1045]Access denied for user 'nobody'@'127.0.0.1' (using password: NO)\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 3. Handshake use_db failure: `[schema:1049]` RFC prefix on both WARN lines.
    let bad_db_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root"))
            .db_name(Some("no_such_db")),
    );
    assert!(mysql_async::Conn::new(bad_db_opts).await.is_err());

    let db_lines = take_lines(&sink, 3).await;
    assert_eq!(
        db_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[WARN] [conn.rs:LINE] [\"open new session or authentication failure\"] [conn=3] [error=\"[schema:1049]Unknown database 'no_such_db'\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=3] [error=\"[schema:1049]Unknown database 'no_such_db'\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 4. Successful connection + clean disconnect.
    let ok_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root")),
    );
    let conn = mysql_async::Conn::new(ok_opts).await.unwrap();
    conn.disconnect().await.unwrap();

    let ok_lines = take_lines(&sink, 3).await;
    assert_eq!(
        ok_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[DEBUG] [conn.rs:LINE] [\"new connection\"] [conn=4] [remoteAddr=127.0.0.1:PORT]",
            "[DEBUG] [conn.rs:LINE] [\"connection closed\"] [conn=4]",
        ]
    );

    let _ = stop_tx.send(());
    srv.await.unwrap();
}
