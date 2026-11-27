//! Golden parity tests for MySQL protocol connection lifecycle, handshake EOF,
//! and authentication failure logs (`specs/D00.3-logging/spec.md` §4.5, Batch 4).
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
    async fn use_db(&mut self, _db: &str) -> Result<(), SqlError> {
        Ok(())
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

    // 1. Raw TCP connect, read initial handshake greeting, then close (EOF).
    let mut raw = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 256];
    let _ = raw.read(&mut buf).await.unwrap();
    drop(raw);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let eof_logs = String::from_utf8(sink.take()).unwrap();
    assert!(
        eof_logs.contains("[\"accept new connection success\"]"),
        "expected accept debug log, got:\n{eof_logs}"
    );
    assert!(
        eof_logs.contains(
            "[\"wait handshake response fail due to connection has be closed by client-side\"]"
        ),
        "expected client-side close debug log, got:\n{eof_logs}"
    );
    assert!(
        eof_logs.contains("[EOF] [conn=1] [\"remote addr\"=127.0.0.1:"),
        "expected Go EOF debug log, got:\n{eof_logs}"
    );

    // 2. Authentication failure (unknown user).
    let bad_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("nobody")),
    );
    assert!(mysql_async::Conn::new(bad_opts).await.is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let auth_logs = String::from_utf8(sink.take()).unwrap();
    assert!(
        auth_logs.contains(
            "[\"open new session or authentication failure\"] [conn=2] [error=\"Access denied for user 'nobody'@'127.0.0.1' (using password: NO)\"]"
        ),
        "expected Go auth failure WARN log, got:\n{auth_logs}"
    );

    // 3. Successful connection + clean disconnect.
    let ok_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root")),
    );
    let conn = mysql_async::Conn::new(ok_opts).await.unwrap();
    conn.disconnect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let ok_logs = String::from_utf8(sink.take()).unwrap();
    assert!(
        ok_logs.contains("[\"new connection\"] [conn=3] [remoteAddr=127.0.0.1:"),
        "expected Go new connection debug log, got:\n{ok_logs}"
    );
    assert!(
        ok_logs.contains("[\"connection closed\"] [conn=3]"),
        "expected Go connection closed debug log, got:\n{ok_logs}"
    );

    let _ = stop_tx.send(());
    srv.await.unwrap();
}
