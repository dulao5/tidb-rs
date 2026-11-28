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

use poc_kv::KvClient;
use poc_session::Factory;
use poc_session::Shared;
use tidb_distsql::DagContext;
use tidb_meta::SchemaCache;
use tidb_mysql_protocol::ConnectionInfo;
use tidb_mysql_protocol::Credential;
use tidb_mysql_protocol::Response;
use tidb_mysql_protocol::SessionFactory;
use tidb_mysql_protocol::SessionHandler;
use tidb_mysql_protocol::Value;

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
    let new_collation = poc_session::detect_new_collation(&kv, &schema)
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
        privileges: None,
    });
    let mut session = factory
        .open_session(&ConnectionInfo {
            connection_id: 1,
            user: "root".to_owned(),
            peer: "127.0.0.1:1".to_owned(),
            auth_user: None,
            auth_host: None,
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

#[tokio::test(flavor = "multi_thread")]
async fn cluster_privilege_sync_auth_and_etcd_watch() {
    let (Ok(pd), Ok(port)) = (
        std::env::var("TIDB_RS_PD"),
        std::env::var("TIDB_RS_TIDB_PORT"),
    ) else {
        eprintln!("TIDB_RS_PD / TIDB_RS_TIDB_PORT not set; skipping");
        return;
    };
    let test_db = "tidb_rs_priv_sync_it";
    let test_user = "tidb_rs_priv_it_u";
    go_sql(
        &port,
        &format!(
            "DROP USER IF EXISTS '{test_user}'@'%'; \
             DROP DATABASE IF EXISTS {test_db}; \
             CREATE DATABASE {test_db}; \
             CREATE TABLE {test_db}.t(id INT PRIMARY KEY, v INT); \
             INSERT INTO {test_db}.t VALUES (1, 10);"
        ),
    );

    let kv = KvClient::connect(vec![pd.clone()]).await.unwrap();
    let schema = SchemaCache::new(kv.clone());
    let new_collation = poc_session::detect_new_collation(&kv, &schema)
        .await
        .unwrap();
    let priv_sync = poc_domain::PrivilegeSync::start(kv.clone(), vec![pd])
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
        privileges: Some(poc_session::privilege::PrivilegeSource::from_sync(
            priv_sync,
        )),
    });

    // open_session must fail closed (1045) if auth_user/auth_host are missing.
    let err = factory
        .open_session(&ConnectionInfo {
            connection_id: 1,
            user: "root".to_owned(),
            peer: "127.0.0.1:1".to_owned(),
            auth_user: None,
            auth_host: None,
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .err()
        .expect("missing auth_user/auth_host must fail closed");
    assert_eq!(err.code, 1045);

    // Existing root account authenticates and can USE test_db.
    let root_auth = factory
        .lookup_account("root", "127.0.0.1")
        .await
        .expect("root account loaded from cluster");
    let mut root_sess = factory
        .open_session(&ConnectionInfo {
            connection_id: 2,
            user: "root".to_owned(),
            peer: "127.0.0.1:2".to_owned(),
            auth_user: root_auth.auth_user,
            auth_host: root_auth.auth_host,
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .unwrap();
    root_sess.use_db(test_db).await.unwrap();

    // Create a user on Go TiDB and verify etcd /tidb/privilege watch propagates it without
    // triggering any unauthenticated reload-on-miss.
    go_sql(
        &port,
        &format!("CREATE USER '{test_user}'@'%' IDENTIFIED BY 'pw123'"),
    );
    let mut found = None;
    for _ in 0..40 {
        if let Some(acc) = factory.lookup_account(test_user, "127.0.0.1").await {
            found = Some(acc);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let acc = found.expect("new user propagated via etcd /tidb/privilege watch");
    assert!(!acc.account_locked);

    let mut u_sess = factory
        .open_session(&ConnectionInfo {
            connection_id: 3,
            user: test_user.to_owned(),
            peer: "127.0.0.1:3".to_owned(),
            auth_user: acc.auth_user,
            auth_host: acc.auth_host,
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .unwrap();
    let err = u_sess.use_db(test_db).await.unwrap_err();
    assert_eq!(err.code, 1044);

    // Grant SELECT and verify live watch reload opens USE test_db.
    go_sql(
        &port,
        &format!("GRANT SELECT ON {test_db}.* TO '{test_user}'@'%'"),
    );
    let mut granted = false;
    for _ in 0..40 {
        if u_sess.use_db(test_db).await.is_ok() {
            granted = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(granted, "GRANT propagated via etcd /tidb/privilege watch");

    // Verify restricted accounts created via Go SQL are handled fail-closed / 3118-locked.
    let u_locked = "tidb_rs_priv_it_lock";
    let u_ssl = "tidb_rs_priv_it_ssl";
    let u_exp = "tidb_rs_priv_it_exp";
    let u_lockpol = "tidb_rs_priv_it_lpol";
    let u_sha2 = "tidb_rs_priv_it_sha2";
    go_sql(
        &port,
        &format!(
            "DROP USER IF EXISTS '{u_locked}'@'%', '{u_ssl}'@'%', '{u_exp}'@'%', '{u_lockpol}'@'%', '{u_sha2}'@'%'; \
             CREATE USER '{u_locked}'@'%' IDENTIFIED BY 'pw123' ACCOUNT LOCK; \
             CREATE USER '{u_ssl}'@'%' IDENTIFIED BY 'pw123' REQUIRE SSL; \
             CREATE USER '{u_exp}'@'%' IDENTIFIED BY 'pw123' PASSWORD EXPIRE; \
             CREATE USER '{u_lockpol}'@'%' IDENTIFIED BY 'pw123' FAILED_LOGIN_ATTEMPTS 3 PASSWORD_LOCK_TIME 2; \
             CREATE USER '{u_sha2}'@'%' IDENTIFIED WITH caching_sha2_password;"
        ),
    );
    let mut saw_locked = false;
    for _ in 0..40 {
        if let Some(acc) = factory.lookup_account(u_locked, "127.0.0.1").await
            && acc.account_locked
        {
            saw_locked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(saw_locked, "locked user propagated via etcd watch");
    for restricted in [u_ssl, u_exp, u_lockpol, u_sha2] {
        assert!(
            factory
                .lookup_account(restricted, "127.0.0.1")
                .await
                .is_none(),
            "restricted account {restricted} must fail closed"
        );
    }

    go_sql(
        &port,
        &format!(
            "DROP USER IF EXISTS '{test_user}'@'%', '{u_locked}'@'%', '{u_ssl}'@'%', '{u_exp}'@'%', '{u_lockpol}'@'%', '{u_sha2}'@'%'; \
             DROP DATABASE IF EXISTS {test_db};"
        ),
    );
}

/// The warning count of a response (Go's OK / EOF packets).
fn warning_count(resp: &Response) -> u16 {
    match resp {
        Response::ResultSet(rs) => rs.warnings,
        Response::Encoded(rs) => rs.warnings,
        Response::Ok(ok) => ok.warnings,
    }
}

/// Prepared statements the prototype answers report Go's `skip prepared plan-cache`
/// warning (#512): at PREPARE (seen through SHOW WARNINGS) and once on every EXECUTE, and
/// the next statement clears it. With `--new-stack=off` (the default).
#[tokio::test(flavor = "multi_thread")]
async fn prepared_uncacheable_statements_warn_as_go() {
    let Ok(pd) = std::env::var("TIDB_RS_PD") else {
        eprintln!("TIDB_RS_PD not set; skipping");
        return;
    };
    let kv = KvClient::connect(vec![pd]).await.unwrap();
    let schema = SchemaCache::new(kv.clone());
    let factory = Factory::new(Shared {
        kv,
        schema,
        dag: DagContext::select_utc(),
        user: "root".to_owned(),
        credential: Credential::native_from_password(""),
        server_version: "8.0.11-TiDB-tidb-rs".to_owned(),
        tls_enabled: false,
        sync: None,
        privileges: None,
    });
    let mut session = factory
        .open_session(&ConnectionInfo {
            connection_id: 2,
            user: "root".to_owned(),
            peer: "127.0.0.1:1".to_owned(),
            auth_user: None,
            auth_host: None,
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .unwrap();
    let show = |rows: Vec<Vec<Value>>| -> Vec<(String, i64, String)> {
        rows.into_iter()
            .map(|r| match &r[..] {
                [Value::Bytes(l), Value::Int(c), Value::Bytes(m)] => (
                    String::from_utf8_lossy(l).into_owned(),
                    *c,
                    String::from_utf8_lossy(m).into_owned(),
                ),
                other => panic!("unexpected SHOW WARNINGS row {other:?}"),
            })
            .collect()
    };
    for (id, sql, reason) in [
        (
            1,
            "SELECT @@max_allowed_packet",
            "query has user-defined variables is un-cacheable",
        ),
        (
            2,
            "SELECT DATABASE()",
            "query has 'database' is un-cacheable",
        ),
        (3, "SELECT USER()", "query has 'user' is un-cacheable"),
    ] {
        let want = vec![(
            "Warning".to_owned(),
            1105,
            format!("skip prepared plan-cache: {reason}"),
        )];
        session.prepare(id, sql).await.unwrap();
        let (_, r) = rows(session.query("SHOW WARNINGS").await.unwrap());
        assert_eq!(show(r), want, "{sql}: PREPARE");
        for _ in 0..2 {
            let resp = session.execute(id, vec![]).await.unwrap();
            assert_eq!(warning_count(&resp), 1, "{sql}: EXECUTE");
            let (_, r) = rows(session.query("SHOW WARNINGS").await.unwrap());
            assert_eq!(show(r.clone()), want, "{sql}: SHOW WARNINGS");
            // SHOW WARNINGS keeps them.
            let (_, again) = rows(session.query("SHOW WARNINGS").await.unwrap());
            assert_eq!(again, r, "{sql}: SHOW WARNINGS twice");
        }
        // The next statement starts without them.
        let resp = session.query("SELECT 1").await.unwrap();
        assert_eq!(warning_count(&resp), 0);
        let (_, r) = rows(session.query("SHOW WARNINGS").await.unwrap());
        assert!(r.is_empty(), "{sql}: after SELECT 1");
    }
    // Cacheable: no warning at PREPARE or EXECUTE.
    session.prepare(4, "SELECT 1").await.unwrap();
    let resp = session.execute(4, vec![]).await.unwrap();
    assert_eq!(warning_count(&resp), 0);
    let (_, r) = rows(session.query("SHOW WARNINGS").await.unwrap());
    assert!(r.is_empty());
}

/// Read-only transactions on the prototype (#547; the transaction context block (D05.4) spec
/// §6): the statements of a transaction read one snapshot while Go TiDB inserts rows between
/// them, as Go v8.5.8's start-ts rules say. `BEGIN` reads its start ts (pessimistic
/// repeatable read), `autocommit = 0` starts the transaction at its first read and `SET
/// autocommit = 1` commits it, pessimistic read committed reads a new ts per statement,
/// optimistic read committed the start ts. A write in a transaction is 1792 and keeps it; the
/// status flags follow. Needs Go TiDB for the inserts.
#[tokio::test(flavor = "multi_thread")]
async fn read_only_transactions_read_one_snapshot() {
    use tidb_mysql_protocol::constants::status::{AUTOCOMMIT, IN_TRANS};
    const TXN_DB: &str = "tidb_rs_session_txn_it";
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
            "DROP DATABASE IF EXISTS {TXN_DB}; CREATE DATABASE {TXN_DB}; \
             CREATE TABLE {TXN_DB}.t(id INT PRIMARY KEY); INSERT INTO {TXN_DB}.t VALUES (1);"
        ),
    );
    let kv = KvClient::connect(vec![pd]).await.unwrap();
    let schema = SchemaCache::new(kv.clone());
    let factory = Factory::new(Shared {
        kv,
        schema,
        dag: DagContext::select_utc(),
        user: "root".to_owned(),
        credential: Credential::native_from_password(""),
        server_version: "8.0.11-TiDB-tidb-rs".to_owned(),
        tls_enabled: false,
        sync: None,
        privileges: None,
    });
    let mut session = factory
        .open_session(&ConnectionInfo {
            connection_id: 3,
            user: "root".to_owned(),
            peer: "127.0.0.1:1".to_owned(),
            auth_user: None,
            auth_host: None,
            database: None,
            collation: 255,
            capability: 0,
            tls_cipher: None,
        })
        .unwrap();
    let select = format!("SELECT id FROM {TXN_DB}.t ORDER BY id");
    let insert = |id: i64| go_sql(&port, &format!("INSERT INTO {TXN_DB}.t VALUES ({id})"));
    macro_rules! ids {
        () => {{
            let (_, r) = rows(session.query(&select).await.unwrap());
            r.into_iter()
                .map(|row| match &row[..] {
                    [Value::Int(id)] => *id,
                    other => panic!("unexpected row {other:?}"),
                })
                .collect::<Vec<i64>>()
        }};
    }
    macro_rules! ok {
        ($sql:expr) => {
            session
                .query($sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {e:?}", $sql));
        };
    }
    assert_eq!(session.status_flags() & (AUTOCOMMIT | IN_TRANS), AUTOCOMMIT);

    // BEGIN: one snapshot until COMMIT; a write is 1792 and the transaction goes on.
    ok!("BEGIN");
    assert_eq!(session.status_flags() & IN_TRANS, IN_TRANS);
    assert_eq!(ids!(), [1]);
    insert(2);
    assert_eq!(ids!(), [1]);
    let e = session
        .query(&format!("INSERT INTO {TXN_DB}.t VALUES (9)"))
        .await
        .unwrap_err();
    assert_eq!((e.code, e.state), (1792, "25006"), "{e:?}");
    let e = session
        .query(&format!("{select} FOR UPDATE"))
        .await
        .unwrap_err();
    assert_eq!(e.code, 1792, "{e:?}");
    assert_eq!(ids!(), [1]);
    ok!("COMMIT");
    assert_eq!(session.status_flags() & IN_TRANS, 0);
    assert_eq!(ids!(), [1, 2]);

    // autocommit = 0: SET does not start a transaction, the first read does; SET autocommit
    // = 1 commits it.
    ok!("SET autocommit = 0");
    assert_eq!(session.status_flags() & (AUTOCOMMIT | IN_TRANS), 0);
    let (_, r) = rows(session.query("SELECT @@autocommit").await.unwrap());
    assert_eq!(r, vec![vec![Value::Int(0)]]);
    assert_eq!(session.status_flags() & IN_TRANS, 0);
    assert_eq!(ids!(), [1, 2]);
    assert_eq!(session.status_flags() & IN_TRANS, IN_TRANS);
    insert(3);
    assert_eq!(ids!(), [1, 2]);
    ok!("SET autocommit = 1");
    assert_eq!(session.status_flags() & (AUTOCOMMIT | IN_TRANS), AUTOCOMMIT);
    assert_eq!(ids!(), [1, 2, 3]);

    // Pessimistic read committed: every statement reads a new ts.
    ok!("SET transaction_isolation = 'READ-COMMITTED'");
    ok!("BEGIN PESSIMISTIC");
    assert_eq!(ids!(), [1, 2, 3]);
    insert(4);
    assert_eq!(ids!(), [1, 2, 3, 4]);
    ok!("ROLLBACK");

    // Optimistic read committed reads the start ts.
    ok!("START TRANSACTION READ ONLY");
    ok!("ROLLBACK");
    ok!("BEGIN OPTIMISTIC");
    assert_eq!(ids!(), [1, 2, 3, 4]);
    insert(5);
    assert_eq!(ids!(), [1, 2, 3, 4]);
    // BEGIN commits the transaction before it and starts a new one.
    ok!("BEGIN");
    assert_eq!(ids!(), [1, 2, 3, 4, 5]);
    ok!("COMMIT");

    go_sql(&port, &format!("DROP DATABASE {TXN_DB}"));
}
