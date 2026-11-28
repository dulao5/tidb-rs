//! Process-level E2E cluster tests for Global KILL (`--schema-sync=true`, `/tidb/server_id/<id>`,
//! bidirectional Go TiDB <-> `tidb-server` `TypeKill`, `1317`, `2013`/disconnect, and client
//! `SHUT_RDWR` detection).
//!
//! Skipped unless `TIDB_RS_PD` (PD address) and `TIDB_RS_TIDB_PORT` (Go TiDB's MySQL port on
//! 127.0.0.1) are set. Creates and drops database `tidb_rs_gkill_it`.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tidb_mysql_protocol::codec::Reader;
use tidb_mysql_protocol::constants::{capability, header};
use tidb_mysql_protocol::handshake::encode_handshake_response;
use tidb_mysql_protocol::packet::PacketIo;
use tokio::net::TcpStream;

const DB: &str = "tidb_rs_gkill_it";

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn cluster() -> Option<(String, u16)> {
    match (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) {
        (Ok(pd), Ok(port)) if !pd.is_empty() && !port.is_empty() => {
            Some((pd, port.parse().expect("TIDB_RS_TIDB_PORT")))
        }
        _ => {
            eprintln!("skipped: set TIDB_RS_PD and TIDB_RS_TIDB_PORT to run");
            None
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_status_connections(status_port: u16) -> u64 {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", status_port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "GET /status HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (_, body) = out.split_once("\r\n\r\n").unwrap();
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["connections"].as_u64().unwrap()
}

#[derive(Debug)]
enum QueryOutcome {
    Ok,
    Rows(Vec<Vec<Option<String>>>),
    ServerErr(u16, String),
    Disconnected(String),
}

struct MiniMysqlClient {
    io: PacketIo<TcpStream>,
    conn_id: u64,
    deprecate_eof: bool,
}

impl MiniMysqlClient {
    async fn connect(port: u16) -> Result<Self, String> {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| format!("connect: {e}"))?;
        let mut io = PacketIo::new(stream, 64 * 1024 * 1024);
        let hs = io
            .read_packet()
            .await
            .map_err(|e| format!("read handshake: {e}"))?;
        // Parse InitialHandshakeV10: [1B protocol][version NUL][4B thread_id]
        let nul = hs[1..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| "bad handshake".to_owned())?;
        let off = 1 + nul + 1;
        let conn_id = u64::from(u32::from_le_bytes([
            hs[off],
            hs[off + 1],
            hs[off + 2],
            hs[off + 3],
        ]));

        let cap = capability::CLIENT_PROTOCOL_41
            | capability::CLIENT_SECURE_CONNECTION
            | capability::CLIENT_PLUGIN_AUTH
            | capability::CLIENT_DEPRECATE_EOF;
        let auth = encode_handshake_response(cap, "root", &[], None, "mysql_native_password");
        io.write_packet(&auth)
            .await
            .map_err(|e| format!("write auth: {e}"))?;
        io.flush().await.map_err(|e| format!("flush auth: {e}"))?;
        let reply = io
            .read_packet()
            .await
            .map_err(|e| format!("read auth reply: {e}"))?;
        if reply.first() != Some(&header::OK) {
            return Err(format!("auth failed: {reply:?}"));
        }
        Ok(Self {
            io,
            conn_id,
            deprecate_eof: true,
        })
    }

    async fn query(&mut self, sql: &str) -> QueryOutcome {
        self.io.reset_sequence();
        let mut cmd = Vec::with_capacity(1 + sql.len());
        cmd.push(0x03); // COM_QUERY
        cmd.extend_from_slice(sql.as_bytes());
        if let Err(e) = self.io.write_packet(&cmd).await {
            return QueryOutcome::Disconnected(e.to_string());
        }
        if let Err(e) = self.io.flush().await {
            return QueryOutcome::Disconnected(e.to_string());
        }
        let first = match self.io.read_packet().await {
            Ok(p) => p,
            Err(e) => return QueryOutcome::Disconnected(e.to_string()),
        };
        match first.first().copied() {
            Some(header::OK) => QueryOutcome::Ok,
            Some(header::ERR) => parse_err_packet(&first),
            _ => {
                let mut r = Reader::new(&first);
                let ncols = r.lenenc_int().ok().flatten().unwrap_or(0) as usize;
                for _ in 0..ncols {
                    if let Err(e) = self.io.read_packet().await {
                        return QueryOutcome::Disconnected(e.to_string());
                    }
                }
                if !self.deprecate_eof
                    && let Err(e) = self.io.read_packet().await
                {
                    return QueryOutcome::Disconnected(e.to_string());
                }
                let mut rows = Vec::new();
                loop {
                    let pkt = match self.io.read_packet().await {
                        Ok(p) => p,
                        Err(e) => return QueryOutcome::Disconnected(e.to_string()),
                    };
                    if pkt.first() == Some(&header::ERR) {
                        return parse_err_packet(&pkt);
                    }
                    if pkt.first() == Some(&header::EOF) && pkt.len() < 9 {
                        break;
                    }
                    rows.push(decode_text_row(&pkt, ncols));
                }
                QueryOutcome::Rows(rows)
            }
        }
    }

    async fn send_com_query_only(&mut self, sql: &str) {
        self.io.reset_sequence();
        let mut cmd = Vec::with_capacity(1 + sql.len());
        cmd.push(0x03);
        cmd.extend_from_slice(sql.as_bytes());
        self.io.write_packet(&cmd).await.unwrap();
        self.io.flush().await.unwrap();
    }

    fn into_stream(self) -> TcpStream {
        self.io.into_parts().0
    }
}

fn parse_err_packet(pkt: &[u8]) -> QueryOutcome {
    let code = if pkt.len() >= 3 {
        u16::from_le_bytes([pkt[1], pkt[2]])
    } else {
        0
    };
    let msg = if pkt.len() >= 9 && pkt[3] == b'#' {
        String::from_utf8_lossy(&pkt[9..]).into_owned()
    } else if pkt.len() > 3 {
        String::from_utf8_lossy(&pkt[3..]).into_owned()
    } else {
        String::new()
    };
    QueryOutcome::ServerErr(code, msg)
}

fn decode_text_row(pkt: &[u8], ncols: usize) -> Vec<Option<String>> {
    let mut r = Reader::new(pkt);
    let mut row = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let cell = r
            .lenenc_str()
            .ok()
            .flatten()
            .map(|b| String::from_utf8_lossy(b).into_owned());
        row.push(cell);
    }
    row
}

async fn exec_ok(port: u16, sql: &str) {
    let mut c = MiniMysqlClient::connect(port).await.unwrap();
    match c.query(sql).await {
        QueryOutcome::Ok | QueryOutcome::Rows(_) => {}
        other => panic!("{sql}: {other:?}"),
    }
}

async fn exec_kill_no_warn(port: u16, sql: &str) {
    let mut c = MiniMysqlClient::connect(port).await.unwrap();
    match c.query(sql).await {
        QueryOutcome::Ok => {}
        other => panic!("{sql}: {other:?}"),
    }
    match c.query("SHOW WARNINGS").await {
        QueryOutcome::Rows(rows) => assert!(rows.is_empty(), "{sql} warnings: {rows:?}"),
        other => panic!("SHOW WARNINGS after {sql}: {other:?}"),
    }
}

async fn query_scalar(port: u16, sql: &str) -> String {
    let mut c = MiniMysqlClient::connect(port).await.unwrap();
    match c.query(sql).await {
        QueryOutcome::Rows(mut r) => r.remove(0).remove(0).unwrap_or_default(),
        other => panic!("{sql}: {other:?}"),
    }
}

struct Node {
    child: Child,
    port: u16,
    status_port: u16,
}

impl Node {
    async fn start(pd: &str, new_stack: &str) -> Node {
        let port = free_port();
        let status_port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_tidb-server"))
            .args([
                "--pd",
                pd,
                "--host",
                "127.0.0.1",
                "-P",
                &port.to_string(),
                "--status-port",
                &status_port.to_string(),
                "--new-stack",
                new_stack,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tidb-server");
        let node = Node {
            child,
            port,
            status_port,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Ok(mut c) = MiniMysqlClient::connect(port).await
                && matches!(c.query("SELECT 1").await, QueryOutcome::Rows(_))
            {
                return node;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        panic!("tidb-server (--new-stack={new_stack}) did not start in time");
    }

    fn stop_and_wait(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }
}

async fn setup_test_tables(go_port: u16) {
    exec_ok(go_port, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    exec_ok(go_port, &format!("CREATE DATABASE {DB}")).await;
    exec_ok(
        go_port,
        &format!("CREATE TABLE {DB}.one_row (id INT PRIMARY KEY, v INT)"),
    )
    .await;
    exec_ok(go_port, &format!("INSERT INTO {DB}.one_row VALUES (1, 10)")).await;
    exec_ok(
        go_port,
        &format!("CREATE TABLE {DB}.slow (id INT PRIMARY KEY, pad VARCHAR(512))"),
    )
    .await;
    exec_ok(
        go_port,
        &format!("CREATE TABLE {DB}.seq16 (n INT PRIMARY KEY)"),
    )
    .await;
    let vals: String = (0..16)
        .map(|i| format!("({i})"))
        .collect::<Vec<_>>()
        .join(",");
    exec_ok(go_port, &format!("INSERT INTO {DB}.seq16 VALUES {vals}")).await;
    exec_ok(
        go_port,
        &format!(
            "INSERT INTO {DB}.slow SELECT a.n + 16*(b.n + 16*(c.n + 16*d.n)), REPEAT('x', 480) \
             FROM {DB}.seq16 a CROSS JOIN {DB}.seq16 b CROSS JOIN {DB}.seq16 c CROSS JOIN {DB}.seq16 d"
        ),
    )
    .await;
}

async fn run_bidirectional_kill_suite(pd: &str, go_port: u16, new_stack: &str) {
    let mut node = Node::start(pd, new_stack).await;

    // Wait for SchemaSync registration in Go TiDB's tidb_servers_info.
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if query_scalar(
            go_port,
            &format!(
                "SELECT COUNT(*) FROM information_schema.tidb_servers_info WHERE port = {}",
                node.port
            ),
        )
        .await
            == "1"
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 1. Verify CONNECTION_ID() on tidb-server encodes non-zero server_id.
    let mut rust_victim = MiniMysqlClient::connect(node.port).await.unwrap();
    let rust_cid: u64 = match rust_victim.query("SELECT CONNECTION_ID()").await {
        QueryOutcome::Rows(mut r) => r.remove(0).remove(0).unwrap().parse().unwrap(),
        other => panic!("CONNECTION_ID(): {other:?}"),
    };
    assert_eq!(rust_cid, rust_victim.conn_id);
    let (rust_gcid, trunc) = tidb_sessionctx::globalconn::parse_conn_id(rust_cid).unwrap();
    assert!(!trunc);
    assert!(rust_gcid.server_id > 0);

    // 2. Cross-node Go TiDB -> tidb-server: KILL QUERY <rust_cid> returns 1317 & keeps conn alive.
    let slow_sql = format!("SELECT id, pad FROM {DB}.slow ORDER BY pad DESC, id DESC");
    let kill_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        exec_kill_no_warn(go_port, &format!("KILL QUERY {rust_cid}")).await;
    });
    let outcome = rust_victim.query(&slow_sql).await;
    kill_task.await.unwrap();
    match outcome {
        QueryOutcome::ServerErr(code, msg) => {
            assert_eq!(code, 1317);
            assert_eq!(msg, "Query execution was interrupted");
        }
        other => {
            panic!("expected 1317 from Go KILL QUERY -> tidb-server ({new_stack}), got {other:?}")
        }
    }
    assert!(matches!(
        rust_victim.query("SELECT 1").await,
        QueryOutcome::Rows(_)
    ));

    // 3. Cross-node Go TiDB -> tidb-server: KILL <rust_cid> severs client connection.
    let kill_conn_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        exec_kill_no_warn(go_port, &format!("KILL {rust_cid}")).await;
    });
    let outcome = rust_victim.query(&slow_sql).await;
    kill_conn_task.await.unwrap();
    match outcome {
        QueryOutcome::Disconnected(reason) => assert!(!reason.is_empty()),
        other => panic!("expected transport disconnect on KILL, got {other:?}"),
    }
    drop(rust_victim);

    // 4. Cross-node tidb-server -> Go TiDB: KILL QUERY <go_cid> returns 1317 on Go TiDB.
    let mut go_victim = MiniMysqlClient::connect(go_port).await.unwrap();
    let go_cid: u64 = match go_victim.query("SELECT CONNECTION_ID()").await {
        QueryOutcome::Rows(mut r) => r.remove(0).remove(0).unwrap().parse().unwrap(),
        other => panic!("Go CONNECTION_ID(): {other:?}"),
    };
    let rust_port = node.port;
    let rust_kill_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        exec_kill_no_warn(rust_port, &format!("KILL QUERY {go_cid}")).await;
    });
    let go_sleep_sql = format!("SELECT SLEEP(20) FROM {DB}.one_row");
    let outcome = go_victim.query(&go_sleep_sql).await;
    rust_kill_task.await.unwrap();
    match outcome {
        QueryOutcome::ServerErr(code, msg) => {
            assert_eq!(code, 1317);
            assert_eq!(msg, "Query execution was interrupted");
        }
        other => panic!("expected 1317 on Go TiDB from tidb-server KILL QUERY, got {other:?}"),
    }
    assert!(matches!(
        go_victim.query("SELECT 1").await,
        QueryOutcome::Rows(_)
    ));

    // 5. Cross-node tidb-server -> Go TiDB: KILL <go_cid> severs Go TiDB connection.
    let rust_kill_conn = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        exec_kill_no_warn(rust_port, &format!("KILL {go_cid}")).await;
    });
    let outcome = go_victim.query(&go_sleep_sql).await;
    rust_kill_conn.await.unwrap();
    match outcome {
        QueryOutcome::Disconnected(reason) => assert!(!reason.is_empty()),
        other => panic!("expected transport disconnect on Go TiDB KILL, got {other:?}"),
    }
    drop(go_victim);

    // Wait for prior connections on tidb-server to settle before testing SHUT_RDWR.
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    while http_status_connections(node.status_port) > 0 && Instant::now() < settle_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(http_status_connections(node.status_port), 0);

    // 6. Client TCP SHUT_RDWR disconnects within ~1-3s and /status connections returns to 0.
    let mut victim = MiniMysqlClient::connect(node.port).await.unwrap();
    victim.send_com_query_only(&slow_sql).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(http_status_connections(node.status_port), 1);

    let std_sock = victim.into_stream().into_std().unwrap();
    let _ = std_sock.shutdown(Shutdown::Both);
    drop(std_sock);

    let disc_deadline = Instant::now() + Duration::from_secs(5);
    while http_status_connections(node.status_port) > 0 && Instant::now() < disc_deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        http_status_connections(node.status_port),
        0,
        "/status connections must return to 0 after client SHUT_RDWR"
    );

    node.stop_and_wait();
}

#[test]
fn global_kill_bidirectional_cluster_e2e() {
    let Some((_pd, go_port)) = cluster() else {
        return;
    };
    let _guard = serial();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let pd = std::env::var("TIDB_RS_PD").unwrap();
    rt.block_on(async {
        setup_test_tables(go_port).await;
        for mode in ["fallback", "off"] {
            run_bidirectional_kill_suite(&pd, go_port, mode).await;
        }
        exec_ok(go_port, &format!("DROP DATABASE IF EXISTS {DB}")).await;
    });
}
