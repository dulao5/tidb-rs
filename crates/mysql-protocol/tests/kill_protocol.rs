//! Protocol-level integration tests for `KILL QUERY`, `KILL CONNECTION`, non-blocking client
//! disconnect probing, and `ServerSessionManager` connection ID lifecycle.

use std::net::Shutdown;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder};
use tidb_mysql_protocol::packet::PacketIo;
use tidb_mysql_protocol::value::encode_text_row;
use tidb_mysql_protocol::{
    ColumnDef, ColumnType, ConnectionInfo, Credential, EncodedResultSet, OkInfo, Param,
    PreparedInfo, Response, ResultSet, Server, ServerConfig, SessionFactory, SessionHandler,
    SqlError, Value,
};
use tidb_server::session_mgr::{ConnControl, ServerSessionManager};
use tidb_sessionctx::session_mgr::SessionManager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone)]
struct KillTestFactory {
    mgr: Arc<ServerSessionManager>,
    mode: TestMode,
}

#[derive(Clone)]
enum TestMode {
    SlowUntilKill {
        saw_probe_disconnect: Arc<AtomicBool>,
        saw_cooperative_abort: Arc<AtomicBool>,
    },
    BigResultSet {
        rows: usize,
        row_len: usize,
    },
    LargeEncoded {
        rows: usize,
        row_len: usize,
    },
}

struct KillTestSession {
    ctrl: Arc<ConnControl>,
    mode: TestMode,
}

impl SessionFactory for KillTestFactory {
    type Session = KillTestSession;

    fn session_manager(&self) -> Option<Arc<ServerSessionManager>> {
        Some(Arc::clone(&self.mgr))
    }

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == "root").then(|| Credential::native_from_password(""))
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<KillTestSession, SqlError> {
        let ctrl = ConnControl::new(info.connection_id);
        Ok(KillTestSession {
            ctrl,
            mode: self.mode.clone(),
        })
    }
}

impl SessionHandler for KillTestSession {
    fn conn_control(&self) -> Option<Arc<ConnControl>> {
        Some(Arc::clone(&self.ctrl))
    }

    fn should_install_connection_alive_during_execute(&self) -> bool {
        matches!(self.mode, TestMode::SlowUntilKill { .. })
    }

    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        if sql.eq_ignore_ascii_case("SELECT 1") {
            return Ok(Response::ResultSet(ResultSet {
                columns: vec![ColumnDef::new("1", ColumnType::LongLong)],
                rows: vec![vec![Value::Int(1)]],
                warnings: 0,
            }));
        }
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
        match &self.mode {
            TestMode::SlowUntilKill {
                saw_probe_disconnect,
                saw_cooperative_abort,
            } => {
                if sql.eq_ignore_ascii_case("CHECK ALIVE") {
                    loop {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        if let Err(err) = self.ctrl.killer.handle_signal() {
                            saw_probe_disconnect.store(true, Ordering::SeqCst);
                            return Err(SqlError::from_tidb_error(&err));
                        }
                    }
                }
                self.ctrl.kill_cancel.cancelled().await;
                let err = self
                    .ctrl
                    .killer
                    .handle_signal()
                    .expect_err("kill signal must be set");
                if sql.eq_ignore_ascii_case("SLEEP FOREVER") {
                    saw_cooperative_abort.store(true, Ordering::SeqCst);
                }
                Err(SqlError::from_tidb_error(&err))
            }
            TestMode::BigResultSet { rows, row_len } => {
                let payload = vec![b'x'; *row_len];
                Ok(Response::ResultSet(ResultSet {
                    columns: vec![ColumnDef::new("c", ColumnType::VarString)],
                    rows: vec![vec![Value::Bytes(payload)]; *rows],
                    warnings: 0,
                }))
            }
            TestMode::LargeEncoded { rows, row_len } => {
                let col = ColumnDef::new("c", ColumnType::VarString);
                let mut col_pkt = Vec::new();
                col.encode(&mut col_pkt);
                let mut row_pkt = Vec::new();
                encode_text_row(
                    &mut row_pkt,
                    std::slice::from_ref(&col),
                    &[Value::Bytes(vec![b'y'; *row_len])],
                )?;
                Ok(Response::Encoded(EncodedResultSet {
                    columns: vec![col],
                    column_packets: vec![col_pkt],
                    rows: vec![row_pkt; *rows],
                    warnings: 0,
                }))
            }
        }
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
async fn kill_query_returns_1317_and_keeps_conn_usable_and_recycles_id() {
    let mgr = ServerSessionManager::new_standalone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let factory = KillTestFactory {
        mgr: Arc::clone(&mgr),
        mode: TestMode::SlowUntilKill {
            saw_probe_disconnect: Arc::new(AtomicBool::new(false)),
            saw_cooperative_abort: Arc::new(AtomicBool::new(false)),
        },
    };
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), factory)
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
    let conn_id = conn.id();
    assert_eq!(u64::from(conn_id), 2_097_154);

    let mgr_clone = Arc::clone(&mgr);
    let killer_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        mgr_clone.kill(u64::from(conn_id), true, false, false);
    });
    let err = conn
        .query_drop("SELECT SLEEP(10)")
        .await
        .expect_err("KILL QUERY must return 1317");
    killer_task.await.unwrap();
    match err {
        mysql_async::Error::Server(se) => assert_eq!(se.code, 1317),
        other => panic!("expected MySQL ERR 1317, got {other:?}"),
    }

    // Connection must still be completely usable for the next query.
    let v: Option<i64> = conn.query_first("SELECT 1").await.unwrap();
    assert_eq!(v, Some(1));

    // Closing the connection unregisters it and recycles its connection ID.
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(2);
    while mgr.client_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mgr.client_count(), 0);
    assert!(mgr.show_process_list().is_empty());

    let _ = stop_tx.send(());
    srv.await.unwrap();
}

#[tokio::test]
async fn kill_connection_aborts_idle_and_active_client() {
    let mgr = ServerSessionManager::new_standalone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let saw_cooperative_abort = Arc::new(AtomicBool::new(false));
    let factory = KillTestFactory {
        mgr: Arc::clone(&mgr),
        mode: TestMode::SlowUntilKill {
            saw_probe_disconnect: Arc::new(AtomicBool::new(false)),
            saw_cooperative_abort: Arc::clone(&saw_cooperative_abort),
        },
    };
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), factory)
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

    // 1. Kill idle connection waiting in read_packet.
    let mut idle_conn = mysql_async::Conn::new(opts.clone()).await.unwrap();
    let idle_id = u64::from(idle_conn.id());
    mgr.kill(idle_id, false, false, false);
    let idle_err = idle_conn
        .query_drop("SELECT 1")
        .await
        .expect_err("killed idle connection must fail");
    assert!(
        !matches!(idle_err, mysql_async::Error::Server(_)),
        "expected transport disconnect, got {idle_err:?}"
    );
    drop(idle_conn);

    // 2. Kill active connection blocked inside query dispatch.
    let mut busy_conn = mysql_async::Conn::new(opts).await.unwrap();
    let busy_id = u64::from(busy_conn.id());
    let mgr_clone = Arc::clone(&mgr);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(40)).await;
        mgr_clone.kill(busy_id, false, false, false);
    });
    let busy_err = busy_conn
        .query_drop("SLEEP FOREVER")
        .await
        .expect_err("KILL CONNECTION must sever active command");
    assert!(
        saw_cooperative_abort.load(Ordering::SeqCst),
        "KILL CONNECTION must let dispatch finish cooperatively with 1317 before socket shutdown"
    );
    assert!(
        !matches!(busy_err, mysql_async::Error::Server(_)),
        "expected transport disconnect, got {busy_err:?}"
    );
    drop(busy_conn);

    let deadline = Instant::now() + Duration::from_secs(2);
    while mgr.client_count() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(mgr.client_count(), 0);
    assert!(mgr.show_process_list().is_empty());

    let _ = stop_tx.send(());
    srv.await.unwrap();
}

async fn run_mid_stream_kill_case(mode: TestMode, max_rows: usize) {
    let mgr = ServerSessionManager::new_standalone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let factory = KillTestFactory {
        mgr: Arc::clone(&mgr),
        mode,
    };
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), factory)
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

    let mut stream_res = conn.query_iter("BIG").await.unwrap();
    let mut seen_rows = 0usize;
    let mut stream_err = None;
    let mut sent_kill = false;
    loop {
        match stream_res.next().await {
            Ok(Some(_row)) => {
                seen_rows += 1;
                if seen_rows == 10 && !sent_kill {
                    sent_kill = true;
                    mgr.kill(conn_id, true, false, false);
                }
            }
            Ok(None) => break,
            Err(e) => {
                stream_err = Some(e);
                break;
            }
        }
    }
    drop(stream_res);
    if !sent_kill {
        mgr.kill(conn_id, true, false, false);
    }
    if let Some(err) = stream_err {
        match err {
            mysql_async::Error::Server(se) => assert_eq!(se.code, 1317),
            other => panic!("mid-write kill must never corrupt packet framing, got {other:?}"),
        }
    }
    assert!(seen_rows <= max_rows);

    let ok: Option<i64> = conn.query_first("SELECT 1").await.unwrap();
    assert_eq!(ok, Some(1));

    drop(conn);
    let _ = stop_tx.send(());
    srv.await.unwrap();
}

#[tokio::test]
async fn kill_query_during_write_result_set_never_tears_packet_frame() {
    run_mid_stream_kill_case(
        TestMode::BigResultSet {
            rows: 20_000,
            row_len: 2048,
        },
        20_000,
    )
    .await;
    run_mid_stream_kill_case(
        TestMode::LargeEncoded {
            rows: 20_000,
            row_len: 2048,
        },
        20_000,
    )
    .await;
}

#[tokio::test]
async fn nonblocking_disconnect_probe_detects_client_shut_rdwr() {
    let mgr = ServerSessionManager::new_standalone();
    let saw_probe_disconnect = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let factory = KillTestFactory {
        mgr: Arc::clone(&mgr),
        mode: TestMode::SlowUntilKill {
            saw_probe_disconnect: Arc::clone(&saw_probe_disconnect),
            saw_cooperative_abort: Arc::new(AtomicBool::new(false)),
        },
    };
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), factory)
            .serve(listener, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });

    let std_stream = std::net::TcpStream::connect(addr).unwrap();
    std_stream.set_nonblocking(true).unwrap();
    let mut stream = tokio::net::TcpStream::from_std(std_stream).unwrap();

    // Read InitialHandshake packet.
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await.unwrap();
    let len = usize::from(hdr[0]) | (usize::from(hdr[1]) << 8) | (usize::from(hdr[2]) << 16);
    let mut hs = vec![0u8; len];
    stream.read_exact(&mut hs).await.unwrap();

    // Minimal HandshakeResponse41 (CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH).
    let cap: u32 = 0x0000_0200 | 0x0000_8000 | 0x0000_2000 | 0x0008_0000;
    let auth = tidb_mysql_protocol::handshake::encode_handshake_response(
        cap,
        "root",
        &[],
        None,
        "mysql_native_password",
    );
    let auth_len = (auth.len() as u32).to_le_bytes();
    stream
        .write_all(&[auth_len[0], auth_len[1], auth_len[2], 1])
        .await
        .unwrap();
    stream.write_all(&auth).await.unwrap();
    stream.flush().await.unwrap();

    // Read OK packet.
    stream.read_exact(&mut hdr).await.unwrap();
    let ok_len = usize::from(hdr[0]) | (usize::from(hdr[1]) << 8) | (usize::from(hdr[2]) << 16);
    let mut ok = vec![0u8; ok_len];
    stream.read_exact(&mut ok).await.unwrap();
    assert_eq!(ok[0], 0x00);

    // Send COM_QUERY "CHECK ALIVE".
    let cmd = b"\x03CHECK ALIVE";
    let cmd_len = (cmd.len() as u32).to_le_bytes();
    stream
        .write_all(&[cmd_len[0], cmd_len[1], cmd_len[2], 0])
        .await
        .unwrap();
    stream.write_all(cmd).await.unwrap();
    stream.flush().await.unwrap();

    // Close peer TCP half cleanly (SHUT_RDWR).
    let std_sock = stream.into_std().unwrap();
    let _ = std_sock.shutdown(Shutdown::Both);
    drop(std_sock);

    // Wait up to 3s for SqlKiller's 1s throttled liveness probe to fire and unregister.
    let deadline = Instant::now() + Duration::from_secs(3);
    while (!saw_probe_disconnect.load(Ordering::SeqCst) || mgr.client_count() > 0)
        && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        saw_probe_disconnect.load(Ordering::SeqCst),
        "SqlKiller 1s probe must detect peer SHUT_RDWR"
    );
    assert_eq!(mgr.client_count(), 0);
    assert!(mgr.show_process_list().is_empty());

    let _ = stop_tx.send(());
    srv.await.unwrap();
}

#[tokio::test]
async fn server_rejects_connections_while_pd_lease_lost_and_accepts_once_restored() {
    let mgr = ServerSessionManager::new_with_config(0, false, true, true);
    assert!(mgr.is_lost_connection_to_pd());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let factory = KillTestFactory {
        mgr: Arc::clone(&mgr),
        mode: TestMode::SlowUntilKill {
            saw_probe_disconnect: Arc::new(AtomicBool::new(false)),
            saw_cooperative_abort: Arc::new(AtomicBool::new(false)),
        },
    };
    let srv = tokio::spawn(async move {
        Server::new(ServerConfig::default(), factory)
            .serve(listener, async move {
                let _ = stop_rx.await;
            })
            .await
            .unwrap();
    });

    // While PD lease is lost, accept loop closes accepted sockets immediately without handshake
    // or ConnID allocation.
    let mut raw = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(2), raw.read(&mut buf))
        .await
        .expect("rejected connection must close promptly")
        .unwrap();
    assert_eq!(n, 0, "expected immediate EOF when PD lease is lost");
    drop(raw);
    assert_eq!(mgr.client_count(), 0);

    // Restore server_id lease: first accepted connection receives local_conn_id=1 -> 2097154
    // (proving rejected connection allocated zero IDs).
    mgr.set_server_id(1, true);
    assert!(!mgr.is_lost_connection_to_pd());
    let opts = Opts::from(
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(addr.port())
            .user(Some("root"))
            .prefer_socket(false),
    );
    let mut conn = mysql_async::Conn::new(opts).await.unwrap();
    assert_eq!(u64::from(conn.id()), 2_097_154);
    let v: Option<i64> = conn.query_first("SELECT 1").await.unwrap();
    assert_eq!(v, Some(1));
    drop(conn);

    // Losing PD lease again causes subsequent connections to be rejected.
    mgr.set_server_id(1, false);
    assert!(mgr.is_lost_connection_to_pd());
    let mut raw2 = tokio::net::TcpStream::connect(addr).await.unwrap();
    let n2 = tokio::time::timeout(Duration::from_secs(2), raw2.read(&mut buf))
        .await
        .expect("rejected connection after PD loss must close promptly")
        .unwrap();
    assert_eq!(n2, 0);

    let _ = stop_tx.send(());
    srv.await.unwrap();
}

#[tokio::test]
async fn kill_connection_unblocks_stalled_packet_writer_with_timeout() {
    let (_peer, server_stream) = tokio::io::duplex(64);
    let mut io = PacketIo::new(server_stream, 16 * 1024 * 1024);
    let tok = tidb_runtime::cancel::ChildToken::new();
    io.set_conn_shutdown(tok.clone()).await;

    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        tok.cancel();
    });

    // Write enough bytes to fill both the 16 KiB BufWriter and the 64-byte duplex buffer so
    // flush blocks until conn_shutdown interrupts it.
    let big = vec![b'z'; 64 * 1024];
    let err = tokio::time::timeout(Duration::from_secs(2), async {
        io.write_packet(&big).await?;
        io.flush().await
    })
    .await
    .expect("blocked packet writer must unblock on conn_shutdown")
    .unwrap_err();
    cancel_task.await.unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}
