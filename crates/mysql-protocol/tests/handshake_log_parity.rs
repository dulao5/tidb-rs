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
    AccountAuth, ConnectionInfo, Credential, OkInfo, Param, PreparedInfo, Response, Server,
    ServerConfig, SessionFactory, SessionHandler, SqlError,
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

    async fn lookup_account(&self, user: &str, _host: &str) -> Option<AccountAuth> {
        match user {
            "root" => Some(AccountAuth::from(Credential::native_from_password(""))),
            "locked" => Some(AccountAuth {
                credential: Credential::native_from_password(""),
                auth_user: Some("locked".to_owned()),
                auth_host: Some("%".to_owned()),
                account_locked: true,
            }),
            _ => None,
        }
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

    // 2. Unknown user (mysql_async sets CLIENT_PLUGIN_AUTH): Go rejects in checkAuthPlugin
    //    before any auth switch with `[server:1045]` and logs `"failed to check the user
    //    authplugin"` + `"Server.onConn handshake"` (never `:677`).
    let bad_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("nobody")),
    );
    assert!(mysql_async::Conn::new(bad_opts).await.is_err());

    let unknown_user_lines = take_lines(&sink, 3).await;
    assert_eq!(
        unknown_user_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[WARN] [conn.rs:LINE] [\"failed to check the user authplugin\"] [conn=2] [error=\"[server:1045]Access denied for user 'nobody'@'127.0.0.1' (using password: NO)\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=2] [error=\"[server:1045]Access denied for user 'nobody'@'127.0.0.1' (using password: NO)\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 3. Existing user with wrong password: passes checkAuthPlugin and fails openSessionAndDoAuth
    //    with `[privilege:1045]` on both `"open new session or authentication failure"` and
    //    `"Server.onConn handshake"`.
    let wrong_pw_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root"))
            .pass(Some("wrong-password")),
    );
    assert!(mysql_async::Conn::new(wrong_pw_opts).await.is_err());

    let wrong_pw_lines = take_lines(&sink, 3).await;
    assert_eq!(
        wrong_pw_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[WARN] [conn.rs:LINE] [\"open new session or authentication failure\"] [conn=3] [error=\"[privilege:1045]Access denied for user 'root'@'127.0.0.1' (using password: YES)\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=3] [error=\"[privilege:1045]Access denied for user 'root'@'127.0.0.1' (using password: YES)\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 4. Handshake use_db failure: `[schema:1049]` RFC prefix on both WARN lines.
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
            "[WARN] [conn.rs:LINE] [\"open new session or authentication failure\"] [conn=4] [error=\"[schema:1049]Unknown database 'no_such_db'\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=4] [error=\"[schema:1049]Unknown database 'no_such_db'\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 5. Partial header during handshake: Go `io.ErrUnexpectedEOF` ("unexpected EOF") ->
    //    DEBUG `"wait handshake response fail"` + WARN `"Server.onConn handshake"` (not EOF DEBUG).
    let mut partial = TcpStream::connect(addr).await.unwrap();
    let _ = partial.read(&mut buf).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut partial, &[0x05, 0x00])
        .await
        .unwrap();
    drop(partial);

    let partial_lines = take_lines(&sink, 3).await;
    assert_eq!(
        partial_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[DEBUG] [conn.rs:LINE] [\"wait handshake response fail\"] [conn=5] [error=\"unexpected EOF\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=5] [error=\"unexpected EOF\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 6. Locked account with valid password: Go checks Account_locked after password verification
    //    and emits `[privilege:3118]` on both handshake WARN lines.
    let locked_opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("locked")),
    );
    assert!(mysql_async::Conn::new(locked_opts).await.is_err());

    let locked_lines = take_lines(&sink, 3).await;
    assert_eq!(
        locked_lines,
        vec![
            "[DEBUG] [server.rs:LINE] [\"accept new connection success\"]",
            "[WARN] [conn.rs:LINE] [\"open new session or authentication failure\"] [conn=6] [error=\"[privilege:3118]Access denied for user 'locked'@'127.0.0.1'. Account is locked.\"]",
            "[WARN] [conn.rs:LINE] [\"Server.onConn handshake\"] [conn=6] [error=\"[privilege:3118]Access denied for user 'locked'@'127.0.0.1'. Account is locked.\"] [\"remote addr\"=127.0.0.1:PORT]",
        ]
    );

    // 7. Successful connection + clean disconnect.
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
            "[DEBUG] [conn.rs:LINE] [\"new connection\"] [conn=7] [remoteAddr=127.0.0.1:PORT]",
            "[DEBUG] [conn.rs:LINE] [\"connection closed\"] [conn=7]",
        ]
    );

    let _ = stop_tx.send(());
    srv.await.unwrap();
}
