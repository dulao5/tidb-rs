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

use poc_domain::SchemaSync;
use poc_domain::ServerInfo;
use poc_domain::SyncConfig;
use poc_kv::KvClient;
use poc_session::Factory;
use poc_session::Session;
use poc_session::Shared;
use tidb_distsql::DagContext;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::ConnectionInfo;
use tidb_mysql_protocol::Credential;
use tidb_mysql_protocol::Response;
use tidb_mysql_protocol::SessionFactory;
use tidb_mysql_protocol::SessionHandler;
use tidb_mysql_protocol::Value;

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
    pd: String,
    port: String,
    kv: KvClient,
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
                auth_user: None,
                auth_host: None,
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
    setup_with(listening_port, |_| {}).await
}

async fn setup_with(listening_port: u32, customize: impl FnOnce(&mut SyncConfig)) -> Option<Env> {
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
    let new_collation = poc_session::detect_new_collation(&kv, &schema)
        .await
        .unwrap();
    let dag = DagContext {
        new_collation_enabled: new_collation,
        ..DagContext::select_utc()
    };
    let info = ServerInfo::new("127.0.0.1".to_owned(), listening_port, 0);
    let mut cfg = SyncConfig::new(vec![pd.clone()], info, dag.clone());
    // A short schema lease keeps the stuck-loop test short (the default is Go's 45 s).
    cfg.limits.schema_lease = Duration::from_secs(5);
    cfg.limits.max_staleness = Duration::from_secs(5);
    customize(&mut cfg);
    let sync = SchemaSync::start(kv.clone(), cfg).await.unwrap();
    assert!(
        sync.wait_admissible(Duration::from_secs(15)).await,
        "synced mode not reached"
    );
    let factory = Factory::new(Shared {
        kv: kv.clone(),
        schema,
        dag,
        sync: Some(sync.clone()),
        user: "root".to_owned(),
        credential: Credential::native_from_password(""),
        server_version: "8.0.11-TiDB-tidb-rs".to_owned(),
        tls_enabled: false,
        privileges: None,
    });
    Some(Env {
        pd,
        port,
        kv,
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

/// (d) A stuck sync loop: within the schema lease the node keeps admitting statements on the
/// cached schema (#136); it deregisters within the staleness bound, so DDL proceeds; it keeps
/// answering correctly on the fallback (ADR 0003) path; once the loop resumes it registers
/// again and returns to synced mode.
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
    // Reloads have stopped, but the lease has not run out: still synced, still registered.
    let admitted = registry.stats().admitted;
    env.same(&mut s, "SELECT * FROM a WHERE id = 2").await;
    assert_eq!(
        registry.stats().admitted,
        admitted + 1,
        "admitted within the lease"
    );
    assert!(registry.is_registered());
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

async fn etcd_get_u64_with_lease(etcd: &mut etcd_client::Client, key: &str) -> Option<(u64, i64)> {
    let resp = etcd.get(key, None).await.unwrap();
    let kv = resp.kvs().first()?;
    let val = std::str::from_utf8(kv.value())
        .unwrap()
        .parse::<u64>()
        .unwrap();
    Some((val, kv.lease()))
}

async fn wait_next_physical_ts(kv: &KvClient, base_ts: u64) -> u64 {
    let base_ms = base_ts >> 18;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ts = kv.current_ts().await.unwrap();
        if (ts >> 18) > base_ms {
            return ts;
        }
        assert!(Instant::now() < deadline, "PD physical TS did not advance");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

struct RestoreSavedSafePoint {
    pd: String,
    prev: Option<String>,
}

impl Drop for RestoreSavedSafePoint {
    fn drop(&mut self) {
        let pd = self.pd.clone();
        let prev = self.prev.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(async move {
                    if let Ok(mut etcd) = etcd_client::Client::connect([pd], None).await {
                        match prev {
                            Some(v) => {
                                let _ = etcd
                                    .put(poc_domain::keys::GC_SAVED_SAFE_POINT, v, None)
                                    .await;
                            }
                            None => {
                                let _ = etcd
                                    .delete(poc_domain::keys::GC_SAVED_SAFE_POINT, None)
                                    .await;
                            }
                        }
                    }
                });
            });
        }
    }
}

struct RestoreGcTidbVars {
    port: String,
    rows: Vec<(String, Option<String>)>,
}

impl Drop for RestoreGcTidbVars {
    fn drop(&mut self) {
        for (name, prev) in &self.rows {
            match prev {
                Some(v) => {
                    let escaped = v.replace('\'', "''");
                    go_sql(
                        &self.port,
                        &format!(
                            "UPDATE mysql.tidb SET variable_value = '{escaped}' WHERE variable_name = '{name}'"
                        ),
                    );
                }
                None => {
                    go_sql(
                        &self.port,
                        &format!("DELETE FROM mysql.tidb WHERE variable_name = '{name}'"),
                    );
                }
            }
        }
    }
}

/// `/tidb/server/minstartts/<ddl_id>` lifecycle on a real cluster:
/// - populated immediately upon `register()` with the registration etcd lease;
/// - pinned to the open read-only transaction's `start_ts` across `BEGIN` and lazy `autocommit=0`
///   reads (verified both via periodic background tick and `report_min_start_ts_now`);
/// - advances once the transaction ends (`COMMIT`, `ROLLBACK`, `COM_RESET_CONNECTION`, `Session` drop);
/// - deleted from etcd on `SchemaSync::shutdown()`.
#[tokio::test(flavor = "multi_thread")]
async fn min_start_ts_etcd_lease_and_transactions_lifecycle() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup_with(39_105, |cfg| {
        cfg.min_start_ts_interval = Duration::from_millis(200);
    })
    .await
    else {
        return;
    };
    let mut etcd = etcd_client::Client::connect([env.pd.clone()], None)
        .await
        .unwrap();
    let ddl_id = env.sync.ddl_id().to_owned();
    let min_key = poc_domain::keys::min_start_ts(&ddl_id);

    // Immediately populated with the same lease as server info.
    let (_, info_lease) = {
        let resp = etcd
            .get(
                format!("{}{ddl_id}", poc_domain::keys::SERVER_INFO_PREFIX),
                None,
            )
            .await
            .unwrap();
        let kv = resp.kvs().first().expect("server info key missing");
        (kv.value().to_vec(), kv.lease())
    };
    let (initial_ts, min_lease) = etcd_get_u64_with_lease(&mut etcd, &min_key)
        .await
        .expect("minstartts key missing immediately after registration");
    assert_ne!(min_lease, 0);
    assert_eq!(min_lease, info_lease);
    assert!(initial_ts > 0);

    let mut s = env.session().await;

    // 1. BEGIN pins minstartts via the periodic background tick and advances on COMMIT.
    s.query("BEGIN").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let begin_ts = s.txn_timestamp();
    assert_ne!(begin_ts, 0);

    let newer_ts = wait_next_physical_ts(&env.kv, begin_ts).await;
    assert!(newer_ts > begin_ts);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((reported, lease)) = etcd_get_u64_with_lease(&mut etcd, &min_key).await
            && reported == begin_ts
        {
            assert_eq!(lease, info_lease);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "periodic tick did not report begin_ts"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    s.query("COMMIT").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let post_commit = env.sync.report_min_start_ts_now().await.unwrap().unwrap();
    assert!(
        post_commit > begin_ts,
        "post_commit={post_commit} <= {begin_ts}"
    );
    assert_eq!(
        etcd_get_u64_with_lease(&mut etcd, &min_key)
            .await
            .unwrap()
            .0,
        post_commit
    );

    // 2. autocommit = 0 does not activate a transaction on SET, only on first read; ROLLBACK releases.
    s.query("SET autocommit = 0").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    assert_eq!(s.txn_timestamp(), 0);
    s.query("SELECT * FROM a WHERE id = 1").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let lazy_ts = s.txn_timestamp();
    assert_ne!(lazy_ts, 0);
    wait_next_physical_ts(&env.kv, lazy_ts).await;
    assert_eq!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap(),
        lazy_ts
    );
    s.query("ROLLBACK").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    assert!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap() > lazy_ts,
        "ROLLBACK must release minstartts guard"
    );

    // 3. COM_RESET_CONNECTION releases an open transaction's minstartts guard.
    s.query("BEGIN").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let reset_ts = s.txn_timestamp();
    wait_next_physical_ts(&env.kv, reset_ts).await;
    assert_eq!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap(),
        reset_ts
    );
    s.reset_connection().await.unwrap();
    assert!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap() > reset_ts,
        "COM_RESET_CONNECTION must release minstartts guard"
    );

    // 4. Session drop releases an open transaction's minstartts guard.
    let mut s2 = env.session().await;
    s2.query("BEGIN").await.unwrap();
    s2.command_done(Duration::ZERO, Duration::ZERO);
    let drop_ts = s2.txn_timestamp();
    wait_next_physical_ts(&env.kv, drop_ts).await;
    assert_eq!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap(),
        drop_ts
    );
    drop(s2);
    assert!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap() > drop_ts,
        "Session drop must release minstartts guard"
    );

    // 5. Shutdown revokes the lease and removes /tidb/server/minstartts/<ddl_id>.
    env.stop().await;
    assert!(etcd_get_u64_with_lease(&mut etcd, &min_key).await.is_none());
}

/// Safe point `CheckVisibility` parity (`9006` `ErrGCTooEarly`) against a real cluster:
/// - `KvClient::connect` loads `/tidb/store/gcworker/saved_safe_point` from etcd;
/// - reads inside an open transaction whose `start_ts` is older than the safe point fail with
///   exact MySQL error `9006` (`HY000`) and Go's exact timestamp message on BOTH point gets
///   (`KvSnapshot::get`) and table scans (`ClientRustStore::check_visibility`), plus the ported
///   store driver (`TikvStore`).
#[tokio::test(flavor = "multi_thread")]
async fn gc_safe_point_check_visibility_9006() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_106).await else {
        return;
    };
    let mut etcd = etcd_client::Client::connect([env.pd.clone()], None)
        .await
        .unwrap();
    let prev_sp = etcd
        .get(poc_domain::keys::GC_SAVED_SAFE_POINT, None)
        .await
        .unwrap()
        .kvs()
        .first()
        .map(|kv| String::from_utf8_lossy(kv.value()).into_owned());
    let _restore = RestoreSavedSafePoint {
        pd: env.pd.clone(),
        prev: prev_sp,
    };

    let mut s = env.session().await;
    s.query("BEGIN").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    let txn_ts = s.txn_timestamp();
    assert_ne!(txn_ts, 0);

    // Advance cluster safe point in etcd past `txn_ts`, then verify a fresh `KvClient::connect`
    // picks it up immediately from etcd AND update the existing `env.kv` cache deterministically.
    let sp = wait_next_physical_ts(&env.kv, txn_ts).await;
    assert!(sp > txn_ts);
    etcd.put(poc_domain::keys::GC_SAVED_SAFE_POINT, sp.to_string(), None)
        .await
        .unwrap();
    let kv2 = KvClient::connect(vec![env.pd.clone()]).await.unwrap();
    assert_eq!(kv2.safe_point_cache().safe_point(), sp);

    // Also test the ported store driver (`TikvStore::get_snapshot`) shares `safe_point_cache`.
    let drv = kv2.store_driver().unwrap();
    let drv_snap = drv.get_snapshot(tidb_kv::version::new_version(txn_ts));
    let drv_key = tidb_distsql::row_key(1, 1);
    let drv_err = drv_snap
        .get(&tidb_kv::Context::background(), &tidb_kv::Key(drv_key), &[])
        .await
        .unwrap_err();
    assert_eq!(drv_err.code(), 9006, "{drv_err}");

    env.kv.safe_point_cache().update(sp, Instant::now());
    let want_msg = format!(
        "GC life time is shorter than transaction duration, transaction starts at {}, GC safe point is {}",
        tidb_store::driver::safepoint::oracle_ts_to_go_time_string(txn_ts),
        tidb_store::driver::safepoint::oracle_ts_to_go_time_string(sp),
    );

    // Point get path (`KvSnapshot::get`) -> exact 9006.
    let err_point = s.query("SELECT v FROM a WHERE id = 1").await.unwrap_err();
    s.command_done(Duration::ZERO, Duration::ZERO);
    assert_eq!(
        (err_point.code, err_point.state, err_point.message.as_str()),
        (9006, "HY000", want_msg.as_str()),
        "{err_point:?}"
    );

    // Table scan / coprocessor path (`ClientRustStore::check_visibility`) -> exact 9006.
    let err_scan = s.query("SELECT * FROM a ORDER BY id").await.unwrap_err();
    s.command_done(Duration::ZERO, Duration::ZERO);
    assert_eq!(
        (err_scan.code, err_scan.state, err_scan.message.as_str()),
        (9006, "HY000", want_msg.as_str()),
        "{err_scan:?}"
    );

    // Committing restores normal reads at newer TSO / MaxTS.
    s.query("COMMIT").await.unwrap();
    s.command_done(Duration::ZERO, Duration::ZERO);
    env.same(&mut s, "SELECT v FROM a WHERE id = 1").await;
    env.same(&mut s, "SELECT count(*) FROM a").await;

    drop(_restore);
    env.stop().await;
}

/// Real Go TiDB GC worker reads `/tidb/server/minstartts/<ddl_id>` and clamps
/// `/tidb/store/gcworker/saved_safe_point` to `<= min_start_ts - 1` when GC runs.
#[tokio::test(flavor = "multi_thread")]
async fn go_gc_worker_respects_minstartts_holdback() {
    let _serial = SERIAL.lock().await;
    let Some(env) = setup(39_107).await else {
        return;
    };
    let mut etcd = etcd_client::Client::connect([env.pd.clone()], None)
        .await
        .unwrap();
    let prev_sp = etcd
        .get(poc_domain::keys::GC_SAVED_SAFE_POINT, None)
        .await
        .unwrap()
        .kvs()
        .first()
        .map(|kv| String::from_utf8_lossy(kv.value()).into_owned());
    let _restore_sp = RestoreSavedSafePoint {
        pd: env.pd.clone(),
        prev: prev_sp.clone(),
    };

    // Snapshot & save existing mysql.tidb GC entries, then trigger Go's GC worker on its next
    // 1-minute tick (`gcWorkerTickInterval`). Note Go clamps `tikv_gc_life_time >= 10m`, so we
    // register an active guard 15 minutes in the past (well within 24h `tidb_gc_max_wait_time`)
    // and verify Go's GC worker clamps `saved_safe_point <= old_ts - 1`.
    let var_names = [
        "tikv_gc_run_interval",
        "tikv_gc_last_run_time",
        "tikv_gc_safe_point",
    ];
    let mut saved_vars = Vec::new();
    for name in var_names {
        let out = go_sql(
            &env.port,
            &format!("SELECT variable_value FROM mysql.tidb WHERE variable_name = '{name}'"),
        );
        let val = out.lines().next().map(str::to_owned);
        saved_vars.push((name.to_owned(), val));
    }
    let _restore_vars = RestoreGcTidbVars {
        port: env.port.clone(),
        rows: saved_vars,
    };

    let now_ts = env.kv.current_ts().await.unwrap();
    let fifteen_mins_ms = 15_u64 * 60 * 1_000;
    let old_ms = (now_ts >> 18).saturating_sub(fifteen_mins_ms);
    let old_ts = old_ms << 18;
    let guard = env.sync.min_start_ts_tracker().register(old_ts).unwrap();
    assert_eq!(
        env.sync.report_min_start_ts_now().await.unwrap().unwrap(),
        old_ts
    );

    go_sql(
        &env.port,
        "INSERT INTO mysql.tidb (variable_name, variable_value) VALUES \
         ('tikv_gc_run_interval', '1s'), \
         ('tikv_gc_last_run_time', '20000101-00:00:00 +0000'), \
         ('tikv_gc_safe_point', '20000101-00:00:00 +0000') \
         ON DUPLICATE KEY UPDATE variable_value = VALUES(variable_value)",
    );

    let deadline = Instant::now() + Duration::from_secs(75);
    let observed_sp = loop {
        if let Some((sp, _)) =
            etcd_get_u64_with_lease(&mut etcd, poc_domain::keys::GC_SAVED_SAFE_POINT).await
            && Some(sp.to_string()) != prev_sp
            && sp > 0
        {
            break sp;
        }
        if Instant::now() >= deadline {
            panic!("Go GC worker did not publish saved_safe_point within 75s");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    let max_allowed = old_ts.saturating_sub(1);
    assert!(
        observed_sp <= max_allowed,
        "Go GC worker advanced saved_safe_point={observed_sp} past minstartts-1={max_allowed}"
    );
    drop(guard);
    env.sync.report_min_start_ts_now().await.unwrap();
    env.stop().await;
}
