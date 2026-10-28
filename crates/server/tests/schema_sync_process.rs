//! The `tidb-server` process in a real cluster (ADR 0005): how long a DDL on Go TiDB waits
//! after the node is killed (`kill -9`: until its etcd lease expires) and after a graceful stop
//! (SIGTERM: it deletes its keys at once), plus the stall-file test hook.
//!
//! Skipped unless `TIDB_RS_PD` (PD address) and `TIDB_RS_TIDB_PORT` (Go TiDB's MySQL port on
//! 127.0.0.1) are set; needs the `mysql` client. The kill -9 case waits for the 90 s lease, so
//! it also needs `TIDB_RS_SLOW=1`. Creates and drops database `tidb_rs_proc_it`. For example:
//!
//! ```sh
//! TIDB_RS_PD=127.0.0.1:27379 TIDB_RS_TIDB_PORT=29000 TIDB_RS_SLOW=1 \
//!     cargo test -p tidb-server --test schema_sync_process -- --nocapture --test-threads 1
//! ```

use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const DB: &str = "tidb_rs_proc_it";

/// The tests share `DB` and measure how long Go TiDB's DDL waits for registered nodes, so they
/// must not run concurrently (a killed node would stall the others' DDL for the lease TTL).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn mysql(port: &str, sql: &str) -> Result<String, String> {
    let out = Command::new("mysql")
        .args([
            "-h",
            "127.0.0.1",
            "-P",
            port,
            "-u",
            "root",
            "-N",
            "-B",
            "-e",
            sql,
        ])
        .output()
        .expect("run mysql client");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn go_sql(port: &str, sql: &str) -> String {
    mysql(port, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn timed_ddl(port: &str, sql: &str) -> Duration {
    let start = Instant::now();
    go_sql(port, sql);
    let took = start.elapsed();
    println!("{took:>10.3?}  {sql}");
    took
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
    fn start(pd: &str, extra: &[&str]) -> Node {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_tidb-server"))
            .args(["--pd", pd, "--host", "127.0.0.1", "-P", &port.to_string()])
            // Several nodes run at once: no fixed status port.
            .args(["--status-port", "0"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start tidb-server");
        Node { child, port }
    }

    /// Whether Go TiDB lists this node in `information_schema.tidb_servers_info`.
    fn listed(&self, go_port: &str) -> bool {
        go_sql(
            go_port,
            &format!(
                "SELECT count(*) FROM information_schema.tidb_servers_info WHERE port = {}",
                self.port
            ),
        )
        .trim()
            == "1"
    }

    fn wait_listed(&self, go_port: &str, want: bool, limit: Duration) -> Duration {
        let start = Instant::now();
        while self.listed(go_port) != want {
            assert!(start.elapsed() < limit, "listed != {want} after {limit:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        start.elapsed()
    }

    /// A query on this node (wait until it answers).
    fn query(&self, sql: &str) -> String {
        let port = self.port.to_string();
        let start = Instant::now();
        loop {
            match mysql(&port, sql) {
                Ok(out) => return out,
                Err(e) => {
                    assert!(start.elapsed() < Duration::from_secs(20), "{sql}: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn signal(&self, sig: &str) {
        let ok = Command::new("kill")
            .args([sig, &self.child.id().to_string()])
            .status()
            .unwrap()
            .success();
        assert!(ok, "kill {sig}");
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cluster() -> Option<(String, String)> {
    match (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) {
        (Ok(pd), Ok(port)) => Some((pd, port)),
        _ => {
            eprintln!("TIDB_RS_PD / TIDB_RS_TIDB_PORT not set; skipping");
            None
        }
    }
}

fn create_table(go: &str) {
    go_sql(
        go,
        &format!(
            "CREATE DATABASE IF NOT EXISTS {DB}; DROP TABLE IF EXISTS {DB}.t; \
             CREATE TABLE {DB}.t(id INT PRIMARY KEY, v INT); INSERT INTO {DB}.t VALUES (1, 1);"
        ),
    );
}

/// Graceful stop: the node deletes its keys, so the next DDL does not wait for it. Also: with
/// `--schema-sync=false` the node never registers.
#[test]
fn graceful_stop_releases_ddl_at_once() {
    let Some((pd, go)) = cluster() else {
        return;
    };
    let _serial = serial();
    create_table(&go);
    let baseline = timed_ddl(&go, &format!("ALTER TABLE {DB}.t ADD COLUMN c1 INT"));

    let node = Node::start(&pd, &[]);
    let took = node.wait_listed(&go, true, Duration::from_secs(20));
    println!("registered after {took:?}");
    assert_eq!(
        node.query(&format!("SELECT * FROM {DB}.t WHERE id = 1")),
        "1\t1\tNULL\n"
    );
    let registered = timed_ddl(&go, &format!("ALTER TABLE {DB}.t ADD COLUMN c2 INT"));
    assert_eq!(
        node.query(&format!("SELECT * FROM {DB}.t WHERE id = 1")),
        "1\t1\tNULL\tNULL\n"
    );

    let stopped = Instant::now();
    node.signal("-TERM");
    let gone = node.wait_listed(&go, false, Duration::from_secs(5));
    let after_stop = timed_ddl(&go, &format!("ALTER TABLE {DB}.t ADD COLUMN c3 INT"));
    let total = stopped.elapsed();
    println!(
        "ADD COLUMN: {baseline:?} unregistered, {registered:?} registered; after SIGTERM: \
         unlisted in {gone:?}, next DDL {after_stop:?} ({total:?} from the signal)"
    );
    assert!(total < Duration::from_secs(3), "{total:?}");

    let off = Node::start(&pd, &["--schema-sync=false"]);
    assert_eq!(
        off.query(&format!("SELECT * FROM {DB}.t WHERE id = 1")),
        "1\t1\tNULL\tNULL\tNULL\n"
    );
    assert!(!off.listed(&go), "--schema-sync=false must not register");
    go_sql(&go, &format!("DROP DATABASE {DB}"));
}

/// The stall-file hook: while the file exists the node deregisters (DDL proceeds) and keeps
/// answering; when it is removed the node registers again.
#[test]
fn stall_file_deregisters_and_recovers() {
    let Some((pd, go)) = cluster() else {
        return;
    };
    let _serial = serial();
    create_table(&go);
    let stall = std::env::temp_dir().join(format!("tidb-rs-stall-{}", std::process::id()));
    let _ = std::fs::remove_file(&stall);
    let node = Node::start(
        &pd,
        &[
            "--schema-sync-stall-file",
            stall.to_str().unwrap(),
            "--schema-sync-max-staleness",
            "2",
        ],
    );
    node.wait_listed(&go, true, Duration::from_secs(20));
    std::fs::write(&stall, b"").unwrap();
    let gone = node.wait_listed(&go, false, Duration::from_secs(10));
    println!("stalled node unlisted after {gone:?} (bound 2 s)");
    timed_ddl(
        &go,
        &format!("ALTER TABLE {DB}.t ADD COLUMN c1 INT DEFAULT 3"),
    );
    assert_eq!(
        node.query(&format!("SELECT * FROM {DB}.t WHERE id = 1")),
        "1\t1\t3\n"
    );
    std::fs::remove_file(&stall).unwrap();
    let back = node.wait_listed(&go, true, Duration::from_secs(20));
    println!("re-registered {back:?} after the stall ended");
    let took = timed_ddl(&go, &format!("ALTER TABLE {DB}.t DROP COLUMN c1"));
    assert!(took < Duration::from_secs(10));
    assert_eq!(
        node.query(&format!("SELECT * FROM {DB}.t WHERE id = 1")),
        "1\t1\n"
    );
    node.signal("-TERM");
    node.wait_listed(&go, false, Duration::from_secs(5));
    go_sql(&go, &format!("DROP DATABASE {DB}"));
}

/// `kill -9`: the node's keys stay until its 90 s lease expires, and DDL waits that long.
#[test]
fn kill_9_blocks_ddl_until_lease_expiry() {
    let Some((pd, go)) = cluster() else {
        return;
    };
    let _serial = serial();
    if std::env::var("TIDB_RS_SLOW").is_err() {
        eprintln!("TIDB_RS_SLOW not set; skipping the 90 s lease wait");
        return;
    }
    create_table(&go);
    let mut node = Node::start(&pd, &[]);
    node.wait_listed(&go, true, Duration::from_secs(20));
    node.query(&format!("SELECT * FROM {DB}.t WHERE id = 1"));
    node.child.kill().unwrap();
    node.child.wait().unwrap();
    let killed = Instant::now();
    let took = timed_ddl(&go, &format!("ALTER TABLE {DB}.t ADD COLUMN c1 INT"));
    println!(
        "after kill -9: DDL took {took:?} ({:?} after the kill)",
        killed.elapsed()
    );
    assert!(
        took > Duration::from_secs(30),
        "the dead node's lease must hold DDL"
    );
    assert!(took < Duration::from_secs(120), "{took:?}");
    assert!(!node.listed(&go));
    go_sql(&go, &format!("DROP DATABASE {DB}"));
}
