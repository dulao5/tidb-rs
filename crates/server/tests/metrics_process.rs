//! The `tidb-server` process in a real cluster (ADR 0006): `/metrics` exposes every metric
//! family the TiDB dashboard panels read, under Go's names, types, label names and buckets,
//! after a small workload; the server info advertises the status port; and Go TiDB's
//! cluster-table fan-out to the node fails as fast with the status server listening as with
//! nothing listening.
//!
//! Skipped unless `TIDB_RS_PD` (PD address) and `TIDB_RS_TIDB_PORT` (Go TiDB's MySQL port on
//! 127.0.0.1) are set; needs the `mysql` client. Creates and drops database
//! `tidb_rs_metrics_it`. For example:
//!
//! ```sh
//! TIDB_RS_PD=127.0.0.1:28379 TIDB_RS_TIDB_PORT=30000 \
//!     cargo test -p tidb-server --test metrics_process -- --nocapture --test-threads 1
//! ```

use std::io::Read;
use std::io::Write;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const DB: &str = "tidb_rs_metrics_it";

/// The tests share `DB` and the nodes they register delay each other's DDL.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn mysql(port: u16, sql: &str) -> Result<String, String> {
    let out = Command::new("mysql")
        .args(["-h", "127.0.0.1", "-P", &port.to_string(), "-u", "root"])
        .args(["-N", "-B", "--show-warnings", "-e", sql])
        .output()
        .expect("run mysql client");
    if out.status.success() {
        // --show-warnings prints the warnings on stderr after a successful statement.
        Ok(String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr))
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn cluster() -> Option<(String, u16)> {
    match (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) {
        (Ok(pd), Ok(port)) => Some((pd, port.parse().expect("TIDB_RS_TIDB_PORT"))),
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

struct Node {
    child: Child,
    port: u16,
}

impl Node {
    fn start(pd: &str, status_port: u16) -> Node {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_tidb-server"))
            .args(["--pd", pd, "--host", "127.0.0.1", "-P", &port.to_string()])
            .args(["--status-port", &status_port.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tidb-server");
        let node = Node { child, port };
        let deadline = Instant::now() + Duration::from_secs(30);
        while mysql(port, "SELECT 1").is_err() {
            assert!(Instant::now() < deadline, "tidb-server did not start");
            std::thread::sleep(Duration::from_millis(200));
        }
        node
    }

    fn query(&self, sql: &str) -> Result<String, String> {
        mysql(self.port, sql)
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

fn http_get(port: u16, path: &str) -> String {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    body.to_owned()
}

fn setup(go: u16) {
    mysql(
        go,
        &format!(
            "DROP DATABASE IF EXISTS {DB}; CREATE DATABASE {DB}; \
             CREATE TABLE {DB}.t (id BIGINT PRIMARY KEY, k INT, c CHAR(20)); \
             INSERT INTO {DB}.t VALUES (1, 1, 'a'), (2, 2, 'b'), (3, 3, 'c'), (500, 5, 'd'), \
             (900, 9, 'e');"
        ),
    )
    .unwrap();
}

/// Every family of the dashboard panels shows up with Go's shape once statements ran through
/// the node, including a region error (the table is split after the node cached its region).
#[test]
fn metrics_expose_every_dashboard_family() {
    let Some((pd, go)) = cluster() else { return };
    let _serial = serial();
    setup(go);
    let status_port = free_port();
    let node = Node::start(&pd, status_port);
    let queries = [
        format!("SELECT * FROM {DB}.t WHERE id = 1"),
        format!("SELECT * FROM {DB}.t WHERE id BETWEEN 1 AND 600"),
        format!("SELECT SUM(k) FROM {DB}.t WHERE id BETWEEN 1 AND 600"),
        format!("SELECT c FROM {DB}.t WHERE id BETWEEN 1 AND 600 ORDER BY c"),
        format!("SELECT DISTINCT c FROM {DB}.t WHERE id BETWEEN 1 AND 600 ORDER BY c"),
    ];
    for q in &queries {
        node.query(q).unwrap();
    }
    assert!(node.query(&format!("SELECT * FROM {DB}.missing")).is_err());
    // Split after the node cached the table's single region: its next reads hit
    // EpochNotMatch (region_err_total, backoff) and reload regions from PD.
    mysql(
        go,
        &format!("SPLIT TABLE {DB}.t BETWEEN (0) AND (100000) REGIONS 4"),
    )
    .unwrap();
    for q in &queries {
        node.query(q).unwrap();
    }
    let text = http_get(status_port, "/metrics");
    let mut problems = tidb_metrics::golden::check(&text, tidb_metrics::golden::TIDB);
    problems.extend(tidb_metrics::golden::check(
        &text,
        tidb_metrics::golden::CLIENT,
    ));
    assert!(problems.is_empty(), "{problems:#?}");
    for want in [
        r#"tidb_server_query_total{resource_group="default",result="OK",type="Query"}"#,
        r#"tidb_server_query_total{resource_group="default",result="Error",type="Query"}"#,
        r#"tidb_server_execute_error_total{db="",resource_group="default",type="schema:1146"}"#,
        r#"tidb_executor_statement_total{db="",resource_group="default",type="Select"}"#,
        r#"tidb_executor_expensive_total{type="HashAggExec"}"#,
        r#"tidb_tikvclient_request_seconds_count{scope="false",stale_read="false",store="#,
        r#"tidb_tikvclient_region_err_total{store="#,
        r#"pd_client_cmd_handle_cmds_duration_seconds_count{type="tso"}"#,
        r#"pd_client_cmd_handle_cmds_duration_seconds_count{type="wait"}"#,
        r#"pd_client_request_handle_requests_duration_seconds_count{type="tso"}"#,
    ] {
        assert!(text.contains(want), "missing {want}");
    }
    let status = http_get(status_port, "/status");
    assert!(status.starts_with(r#"{"connections":"#), "{status}");

    // The server info advertises the status port.
    let advertised = mysql(
        go,
        &format!(
            "SELECT status_port FROM information_schema.tidb_servers_info WHERE port = {}",
            node.port
        ),
    )
    .unwrap();
    assert_eq!(advertised.trim(), status_port.to_string());
    drop(node);
    mysql(go, &format!("DROP DATABASE {DB}")).unwrap();
}

/// Go TiDB's cluster tables fan out to every registered TiDB's status port (TiDB coprocessor
/// gRPC, HTTP `/config`, diagnostics gRPC). A listening status server must not make them
/// slower or fail differently than a status port where nothing listens (`--status-port 0`,
/// the behaviour before the status server existed). Both end in a warning for this node;
/// the coprocessor-based tables (`cluster_processlist`, ...) take about 40 s either way
/// because Go's client retries the failed TiDB RPC with backoff (see ADR 0006).
#[test]
fn cluster_tables_no_slower_with_status_server() {
    let Some((pd, go)) = cluster() else { return };
    let _serial = serial();
    let queries = [
        "SELECT COUNT(*) FROM information_schema.cluster_processlist",
        "SELECT COUNT(*) FROM information_schema.cluster_config WHERE type = 'tidb'",
        "SELECT COUNT(*) FROM information_schema.cluster_load WHERE type = 'tidb'",
        "SELECT COUNT(*) FROM information_schema.cluster_info",
    ];
    let mut took = Vec::new();
    for (label, status_port) in [("listening", free_port()), ("port 0", 0)] {
        let node = Node::start(&pd, status_port);
        for q in queries {
            let start = Instant::now();
            let out = mysql(go, q).unwrap_or_else(|e| panic!("{label}: {q}: {e}"));
            let elapsed = start.elapsed();
            let warning = out.lines().find(|l| l.contains("Warning")).unwrap_or("");
            println!("{label:>10} {elapsed:>10.3?}  {q}\n{:>22}{warning}", "");
            took.push(elapsed);
        }
        drop(node);
    }
    let (listening, none) = took.split_at(queries.len());
    for ((q, l), n) in queries.iter().zip(listening).zip(none) {
        assert!(
            *l <= n.mul_f64(1.25) + Duration::from_secs(2),
            "{q}: {l:?} with the status server, {n:?} without"
        );
    }
}
