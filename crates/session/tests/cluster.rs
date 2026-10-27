//! Sessions against a real cluster: the optimistic schema check across a DDL.
//!
//! Skipped unless `TIDB_RS_PD` (PD address) and `TIDB_RS_TIDB_PORT` (Go TiDB's MySQL port on
//! 127.0.0.1) are set; needs the `mysql` client. Creates and drops database
//! `tidb_rs_session_it`. For example:
//!
//! ```sh
//! TIDB_RS_PD=127.0.0.1:23379 TIDB_RS_TIDB_PORT=25000 \
//!     cargo test -p tidb-session --test cluster -- --nocapture
//! ```

use std::process::Command;

use tidb_distsql::DagContext;
use tidb_kv::KvClient;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::ConnectionInfo;
use tidb_mysql_protocol::Credential;
use tidb_mysql_protocol::Response;
use tidb_mysql_protocol::SessionFactory;
use tidb_mysql_protocol::SessionHandler;
use tidb_mysql_protocol::Value;
use tidb_session::Factory;
use tidb_session::Shared;

const DB: &str = "tidb_rs_session_it";

fn go_sql(port: &str, sql: &str) {
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
    assert!(
        out.status.success(),
        "{sql}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn rows(resp: Response) -> (Vec<String>, Vec<Vec<Value>>) {
    match resp {
        Response::ResultSet(rs) => (rs.columns.into_iter().map(|c| c.name).collect(), rs.rows),
        other => panic!("expected a result set, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn first_statement_after_ddl_takes_the_checked_path() {
    let (Ok(pd), Ok(port)) = (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) else {
        eprintln!("TIDB_RS_PD / TIDB_RS_TIDB_PORT not set; skipping");
        return;
    };
    go_sql(
        &port,
        &format!(
            "DROP DATABASE IF EXISTS {DB}; CREATE DATABASE {DB}; \
             CREATE TABLE {DB}.t(id INT PRIMARY KEY, v INT); \
             INSERT INTO {DB}.t VALUES (1, 10), (2, 20);"
        ),
    );
    let kv = KvClient::connect(vec![pd]).await.unwrap();
    let schema = SchemaCache::new(kv.clone());
    let new_collation = tidb_session::detect_new_collation(&kv, &schema)
        .await
        .unwrap();
    let factory = Factory::new(Shared {
        kv,
        schema,
        dag: DagContext {
            new_collation_enabled: new_collation,
            ..DagContext::select_utc()
        },
        user: "root".to_owned(),
        credential: Credential::native_from_password(""),
        server_version: "8.0.11-TiDB-tidb-rs".to_owned(),
        tls_enabled: false,
        sync: None,
    });
    let mut session = factory
        .open_session(&ConnectionInfo {
            connection_id: 1,
            user: "root".to_owned(),
            peer: "127.0.0.1:1".to_owned(),
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .unwrap();
    let stats = || factory.shared().schema.stats();
    let point = format!("SELECT * FROM {DB}.t WHERE id = 1");

    // Warm: planned with the cached schema, confirmed by the concurrent version check.
    let (cols, r) = rows(session.query(&point).await.unwrap());
    assert_eq!(cols, ["id", "v"]);
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(10)]]);
    let before = stats();
    let (_, r) = rows(session.query(&point).await.unwrap());
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(10)]]);
    let after = stats();
    assert_eq!(after.checks_passed, before.checks_passed + 1, "{after:?}");
    assert_eq!(after.checks_failed, before.checks_failed);

    // A DDL between statements. The next statement still plans with the old cached schema
    // (two columns) and reads with it, but the version check at its start_ts rejects that
    // result: it is re-planned with the new schema and returns three columns.
    go_sql(
        &port,
        &format!("ALTER TABLE {DB}.t ADD COLUMN w INT NOT NULL DEFAULT 7"),
    );
    let before = stats();
    let (cols, r) = rows(session.query(&point).await.unwrap());
    let after = stats();
    println!("after ADD COLUMN: {before:?} -> {after:?}");
    assert_eq!(cols, ["id", "v", "w"]);
    assert_eq!(r, vec![vec![Value::Int(1), Value::Int(10), Value::Int(7)]]);
    assert_eq!(after.checks_failed, before.checks_failed + 1, "{after:?}");
    assert_eq!(after.full_loads, before.full_loads + 1, "{after:?}");

    // The new schema is cached now: the following statement is confirmed again.
    let before = stats();
    let (cols, _) = rows(
        session
            .query(&format!("SELECT w FROM {DB}.t WHERE id = 2"))
            .await
            .unwrap(),
    );
    assert_eq!(cols, ["w"]);
    let after = stats();
    assert_eq!(after.checks_passed, before.checks_passed + 1, "{after:?}");
    assert_eq!(after.checks_failed, before.checks_failed);

    // Planning errors with the cached schema are not trusted either: a table created after
    // the newest cached version is found through the checked path.
    go_sql(&port, &format!("CREATE TABLE {DB}.u(id INT PRIMARY KEY)"));
    let (cols, r) = rows(
        session
            .query(&format!("SELECT id FROM {DB}.u"))
            .await
            .unwrap(),
    );
    assert_eq!(cols, ["id"]);
    assert!(r.is_empty());

    go_sql(&port, &format!("DROP DATABASE {DB}"));
}
