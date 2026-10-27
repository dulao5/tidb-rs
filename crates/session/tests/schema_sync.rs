//! Synced mode (ADR 0005) against a real cluster: DDL issued on Go TiDB while this process is
//! registered as a TiDB server, the metadata-lock wait for an in-flight statement, and the
//! self-deregistration of a stuck sync loop.
//!
//! Skipped unless `TIDB_RS_PD` (PD address) and `TIDB_RS_TIDB_PORT` (Go TiDB's MySQL port on
//! 127.0.0.1) are set; needs the `mysql` client. Creates and drops database
//! `tidb_rs_sync_it`. The tests register in the cluster's etcd and run one at a time. For
//! example:
//!
//! ```sh
//! TIDB_RS_PD=127.0.0.1:27379 TIDB_RS_TIDB_PORT=29000 \
//!     cargo test -p tidb-session --test schema_sync -- --nocapture
//! ```

use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use tidb_distsql::DagContext;
use tidb_domain::SchemaSync;
use tidb_domain::ServerInfo;
use tidb_domain::SyncConfig;
use tidb_kv::KvClient;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::ConnectionInfo;
use tidb_mysql_protocol::Credential;
use tidb_mysql_protocol::Response;
use tidb_mysql_protocol::SessionFactory;
use tidb_mysql_protocol::SessionHandler;
use tidb_mysql_protocol::Value;
use tidb_session::Factory;
use tidb_session::Session;
use tidb_session::Shared;

const DB: &str = "tidb_rs_sync_it";

/// The tests register in etcd and issue DDL: run them one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn mysql_cmd(port: &str, sql: &str) -> Command {
    let mut cmd = Command::new("mysql");
    cmd.args([
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
    ]);
    cmd
}

fn go_sql(port: &str, sql: &str) -> String {
    let out = mysql_cmd(port, sql).output().expect("run mysql client");
    assert!(
        out.status.success(),
        "{sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `sql` on Go TiDB in the background.
fn go_spawn(port: &str, sql: &str) -> Child {
    mysql_cmd(port, sql)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run mysql client")
}

/// Wait up to `limit` for `child` to exit successfully; `None` if it is still running.
async fn wait_child(child: &mut Child, limit: Duration) -> Option<Duration> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "DDL failed: {status}");
            return Some(start.elapsed());
        }
        if start.elapsed() >= limit {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn timed_ddl(port: &str, sql: &str) -> Duration {
    let start = Instant::now();
    go_sql(port, sql);
    let took = start.elapsed();
    println!("{took:>10.3?}  {sql}");
    took
}

fn rows(resp: Response) -> (Vec<String>, Vec<Vec<Value>>) {
    match resp {
        Response::ResultSet(rs) => (rs.columns.into_iter().map(|c| c.name).collect(), rs.rows),
        other => panic!("expected a result set, got {other:?}"),
    }
}

/// Rows as Go's `mysql -B` prints them.
fn tsv(rows: &[Vec<Value>]) -> String {
    rows.iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    Value::Null => "NULL".to_owned(),
                    Value::Int(i) => i.to_string(),
                    Value::UInt(u) => u.to_string(),
                    Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    Value::Decimal(d) => d.clone(),
                    Value::Double(d) => d.to_string(),
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join("\t")
                + "\n"
        })
        .collect()
}

struct Env {
    port: String,
    factory: Factory,
    sync: SchemaSync,
}

impl Env {
    async fn session(&self) -> Session {
        let mut s = self
            .factory
            .open_session(&ConnectionInfo {
                connection_id: 1,
                user: "root".to_owned(),
                peer: "127.0.0.1:1".to_owned(),
                database: Some(DB.to_owned()),
                collation: 255,
                capability: 0,
                tls_cipher: None,
            })
            .unwrap();
        s.use_db(DB).await.unwrap();
        s
    }

    /// Query on tidb-rs and on Go TiDB; assert both return the same rows.
    async fn same(&self, session: &mut Session, sql: &str) -> Vec<Vec<Value>> {
        let (_, r) = rows(session.query(sql).await.unwrap());
        session.command_done(Duration::ZERO, Duration::ZERO);
        assert_eq!(
            tsv(&r),
            go_sql(&self.port, &format!("USE {DB}; {sql}")),
            "{sql}"
        );
        r
    }

    async fn stop(self) {
        self.sync.shutdown().await;
    }
}

/// `None` (test skipped) unless the cluster is configured. `listening_port` only has to be
/// unique among the cluster's TiDB servers (nothing listens on it).
async fn setup(listening_port: u32) -> Option<Env> {
    let (Ok(pd), Ok(port)) = (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) else {
        eprintln!("TIDB_RS_PD / TIDB_RS_TIDB_PORT not set; skipping");
        return None;
    };
    go_sql(
        &port,
        &format!(
            "DROP DATABASE IF EXISTS {DB}; CREATE DATABASE {DB}; \
             CREATE TABLE {DB}.a(id INT PRIMARY KEY, v INT); \
             CREATE TABLE {DB}.b(id INT PRIMARY KEY, v INT); \
             INSERT INTO {DB}.a VALUES (1, 10), (2, 20), (3, 30); \
             INSERT INTO {DB}.b VALUES (1, 100);"
        ),
    );
    let kv = KvClient::connect(vec![pd.clone()]).await.unwrap();
    let schema = SchemaCache::new(kv.clone());
    let new_collation = tidb_session::detect_new_collation(&kv, &schema)
        .await
        .unwrap();
    let dag = DagContext {
        new_collation_enabled: new_collation,
        ..DagContext::select_utc()
    };
    let info = ServerInfo::new("127.0.0.1".to_owned(), listening_port, 0);
    let sync = SchemaSync::start(kv.clone(), SyncConfig::new(vec![pd], info, dag.clone()))
        .await
        .unwrap();
    assert!(
        sync.wait_admissible(Duration::from_secs(15)).await,
        "synced mode not reached"
    );
    let factory = Factory::new(Shared {
        kv,
        schema,
        dag,
        sync: Some(sync.clone()),
        user: "root".to_owned(),
        credential: Credential::native_from_password(""),
        server_version: "8.0.11-TiDB-tidb-rs".to_owned(),
        tls_enabled: false,
    });
    Some(Env {
        port,
        factory,
        sync,
    })
}

/// (a) ADD COLUMN, ADD INDEX, MODIFY COLUMN (type change, reorg) and DROP COLUMN issued on Go
/// TiDB complete while this node is registered; synced statements before and after each DDL
/// return what Go TiDB returns, and the new schema is in use as soon as the DDL returns.
#[tokio::test(flavor = "multi_thread")]
async fn ddl_completes_and_results_follow() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_101).await else {
        return;
    };
    let mut s = env.session().await;
    let queries = [
        "SELECT * FROM a WHERE id = 2",
        "SELECT * FROM a ORDER BY id",
        "SELECT * FROM a WHERE id BETWEEN 2 AND 3 ORDER BY id",
        "SELECT count(*) FROM a",
    ];
    for q in queries {
        env.same(&mut s, q).await;
    }
    let before = env.sync.registry().stats();
    for ddl in [
        "ALTER TABLE a ADD COLUMN c INT NOT NULL DEFAULT 7",
        "ALTER TABLE a ADD INDEX iv(v)",
        "ALTER TABLE a MODIFY COLUMN v VARCHAR(20)",
        "ALTER TABLE a DROP COLUMN c",
    ] {
        let took = timed_ddl(&env.port, &format!("USE {DB}; {ddl}"));
        assert!(took < Duration::from_secs(20), "{ddl} took {took:?}");
        for q in queries {
            env.same(&mut s, q).await;
        }
    }
    // Right after the ADD COLUMN returned, the column was visible (checked by `same` above
    // through Go's output); after the DROP it is gone.
    let (cols, _) = rows(s.query("SELECT * FROM a WHERE id = 1").await.unwrap());
    assert_eq!(cols, ["id", "v"]);
    let after = env.sync.registry().stats();
    println!("{before:?}\n{after:?}");
    assert!(after.admitted > before.admitted, "synced mode used");
    assert_eq!(after.invalidated, 0);
    assert_eq!(after.deregistrations, 0);
    assert!(
        after.mdl_reports >= before.mdl_reports + 4,
        "one report per step"
    );
    env.stop().await;
}

/// (b) A statement still in flight with an old schema version on table `a` holds back a DDL on
/// `a` until it ends (its response is written: `command_done`); a DDL on `b` is not held.
#[tokio::test(flavor = "multi_thread")]
async fn in_flight_statement_holds_back_ddl_on_its_table_only() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_102).await else {
        return;
    };
    let mut s = env.session().await;
    let (_, r) = rows(s.query("SELECT * FROM a ORDER BY id").await.unwrap());
    assert_eq!(r.len(), 3);
    // `s` has not finished writing its response: it keeps its MDL entry.
    assert_eq!(env.sync.registry().in_flight(), 1);

    let mut ddl_a = go_spawn(&env.port, &format!("ALTER TABLE {DB}.a ADD COLUMN x INT"));
    let mut ddl_b = go_spawn(&env.port, &format!("ALTER TABLE {DB}.b ADD COLUMN x INT"));
    let b_took = wait_child(&mut ddl_b, Duration::from_secs(20)).await;
    println!("DDL on b: {b_took:?}");
    assert!(b_took.is_some(), "DDL on another table must not wait");
    assert!(
        wait_child(&mut ddl_a, Duration::from_secs(3))
            .await
            .is_none(),
        "DDL on a must wait for the in-flight statement"
    );
    // Other statements keep running meanwhile, on the newest loaded schema.
    let mut s2 = env.session().await;
    env.same(&mut s2, "SELECT * FROM b").await;

    let released = Instant::now();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let a_took = wait_child(&mut ddl_a, Duration::from_secs(20)).await;
    println!("DDL on a after the statement ended: {a_took:?}");
    assert!(
        a_took.is_some(),
        "DDL on a must finish once the statement ended"
    );
    assert!(released.elapsed() < Duration::from_secs(20));
    env.same(&mut s2, "SELECT * FROM a ORDER BY id").await;
    assert_eq!(env.sync.registry().stats().deregistrations, 0);
    env.stop().await;
}

/// (d) A stuck sync loop: the node deregisters within the staleness bound, so DDL proceeds;
/// it keeps answering correctly on the fallback (ADR 0003) path; once the loop resumes it
/// registers again and returns to synced mode.
#[tokio::test(flavor = "multi_thread")]
async fn stuck_sync_loop_deregisters_and_falls_back() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_103).await else {
        return;
    };
    let registry = env.sync.registry().clone();
    let bound = registry.limits().max_staleness;
    let mut s = env.session().await;
    env.same(&mut s, "SELECT * FROM a WHERE id = 1").await;

    let stalled = Instant::now();
    env.sync.set_stalled(true);
    // A DDL started now completes once the node has deregistered.
    let ddl = format!("ALTER TABLE {DB}.a ADD COLUMN c INT NOT NULL DEFAULT 5");
    let mut child = go_spawn(&env.port, &ddl);
    while registry.is_registered() {
        assert!(stalled.elapsed() < bound + Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let deregistered = stalled.elapsed();
    assert!(
        wait_child(&mut child, Duration::from_secs(30))
            .await
            .is_some(),
        "DDL must not be blocked by a stuck node"
    );
    let ddl_took = stalled.elapsed();
    println!(
        "stalled: deregistered after {deregistered:?}, DDL done {ddl_took:?} after the stall began (bound {bound:?})"
    );
    assert!(ddl_took < bound + Duration::from_secs(5));

    // Fallback path: correct results with the new column, no synced admissions.
    let admitted = registry.stats().admitted;
    let r = env.same(&mut s, "SELECT * FROM a WHERE id = 1").await;
    assert_eq!(r[0].len(), 3);
    env.same(&mut s, "SELECT * FROM a ORDER BY id").await;
    assert_eq!(registry.stats().admitted, admitted, "no synced statements");

    // Resume: re-registers, synced again.
    env.sync.set_stalled(false);
    assert!(env.sync.wait_admissible(Duration::from_secs(15)).await);
    let stats = registry.stats();
    assert_eq!((stats.registrations, stats.deregistrations), (2, 1));
    env.same(&mut s, "SELECT * FROM a WHERE id = 3").await;
    assert_eq!(registry.stats().admitted, admitted + 1);
    timed_ddl(&env.port, &format!("ALTER TABLE {DB}.a DROP COLUMN c"));
    env.same(&mut s, "SELECT * FROM a ORDER BY id").await;
    env.stop().await;
}

/// Restores `tidb_enable_metadata_lock` on drop.
struct MdlOff(String);

impl Drop for MdlOff {
    fn drop(&mut self) {
        go_sql(&self.0, "SET GLOBAL tidb_enable_metadata_lock = ON");
    }
}

/// With `tidb_enable_metadata_lock=OFF` the owner waits on `all_schema_versions/<ddl_id>`
/// instead of per-job reports. The node advances that key only once no in-flight statement
/// uses an older schema (on any table: there is no table list without MDL), so the two-version
/// bound still holds for its statements.
#[tokio::test(flavor = "multi_thread")]
async fn mdl_off_waits_for_older_statements() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_104).await else {
        return;
    };
    go_sql(&env.port, "SET GLOBAL tidb_enable_metadata_lock = OFF");
    let _restore = MdlOff(env.port.clone());
    let mut s = env.session().await;
    let (_, r) = rows(s.query("SELECT * FROM a ORDER BY id").await.unwrap());
    assert_eq!(r.len(), 3);
    let mut ddl = go_spawn(&env.port, &format!("ALTER TABLE {DB}.b ADD COLUMN x INT"));
    assert!(
        wait_child(&mut ddl, Duration::from_secs(3)).await.is_none(),
        "without MDL, any DDL waits for the older in-flight statement"
    );
    s.command_done(Duration::ZERO, Duration::ZERO);
    let took = wait_child(&mut ddl, Duration::from_secs(20)).await;
    println!("MDL off: DDL finished {took:?} after the statement ended");
    assert!(took.is_some());
    let mut s2 = env.session().await;
    env.same(&mut s2, "SELECT * FROM b").await;
    drop(_restore);
    env.stop().await;
}
