//! End-to-end tests: a real server on a loopback port, driven by `mysql_async` (a real MySQL
//! client library) and by a minimal hand-written client for exact packet checks.

use std::net::SocketAddr;

use mysql_async::prelude::*;
use mysql_async::{Opts, OptsBuilder, Row};
use tidb_mysql_protocol::constants::{auth_plugin, capability::*, command, status};
use tidb_mysql_protocol::packet::PacketIo;
use tidb_mysql_protocol::{
    ColumnDef, ColumnType, ConnectionInfo, Credential, DateTime, Duration, Param, PreparedInfo,
    Response, ResultSet, Server, ServerConfig, SessionFactory, SessionHandler, SqlError, Value,
};
use tokio::net::{TcpListener, TcpStream};

const PASSWORD: &str = "s3cret";

struct Factory;

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        match user {
            "root" => Some(Credential::native_from_password("")),
            "alice" => Some(Credential::native_from_password(PASSWORD)),
            _ => None,
        }
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<Session, SqlError> {
        Ok(Session {
            db: info.database.clone(),
            stmts: Default::default(),
        })
    }
}

struct Session {
    db: Option<String>,
    stmts: std::collections::HashMap<u32, String>,
}

/// One row exercising most column types.
fn typed_result() -> ResultSet {
    let mut dec = ColumnDef::new("dec", ColumnType::NewDecimal);
    dec.decimals = 2;
    let mut dt = ColumnDef::new("dt", ColumnType::Datetime);
    dt.decimals = 6;
    ResultSet {
        columns: vec![
            ColumnDef::new("i", ColumnType::LongLong),
            ColumnDef::new("t", ColumnType::Tiny),
            ColumnDef::new("d", ColumnType::Double),
            ColumnDef::new("s", ColumnType::VarString),
            dec,
            dt,
            ColumnDef::new("day", ColumnType::Date),
            ColumnDef::new("tm", ColumnType::Duration),
            ColumnDef::new("n", ColumnType::LongLong),
        ],
        rows: vec![vec![
            Value::Int(-42),
            Value::Int(7),
            Value::Double(2.5),
            Value::from("héllo"),
            Value::Decimal("12.34".into()),
            Value::DateTime(DateTime {
                year: 2024,
                month: 2,
                day: 29,
                hour: 13,
                minute: 14,
                second: 15,
                microsecond: 123_456,
            }),
            Value::DateTime(DateTime::date(1999, 12, 31)),
            Value::Time(Duration {
                negative: true,
                hours: 25,
                minutes: 1,
                seconds: 2,
                microseconds: 0,
            }),
            Value::Null,
        ]],
        warnings: 0,
    }
}

impl SessionHandler for Session {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        match sql.trim().to_ascii_lowercase().as_str() {
            "select 1" => Ok(Response::ResultSet(ResultSet {
                columns: vec![ColumnDef::new("1", ColumnType::LongLong)],
                rows: vec![vec![Value::Int(1)]],
                warnings: 0,
            })),
            "select many" => Ok(Response::ResultSet(ResultSet {
                columns: vec![ColumnDef::new("n", ColumnType::LongLong)],
                rows: (0..1000).map(|i| vec![Value::Int(i)]).collect(),
                warnings: 0,
            })),
            "select big" => Ok(Response::ResultSet(ResultSet {
                columns: vec![ColumnDef::new("b", ColumnType::LongBlob)],
                // Larger than one 16 MiB packet.
                rows: vec![vec![Value::Bytes(vec![b'x'; (1 << 24) + 100])]],
                warnings: 0,
            })),
            "select types" => Ok(Response::ResultSet(typed_result())),
            "select database()" => Ok(Response::ResultSet(ResultSet {
                columns: vec![ColumnDef::new("database()", ColumnType::VarString)],
                rows: vec![vec![self.db.clone().map_or(Value::Null, Value::from)]],
                warnings: 0,
            })),
            "set names utf8mb4" => Ok(Response::ok()),
            // Sent by mysql_async right after connecting.
            "select @@max_allowed_packet,@@wait_timeout" => Ok(Response::ResultSet(ResultSet {
                columns: vec![
                    ColumnDef::new("@@max_allowed_packet", ColumnType::LongLong),
                    ColumnDef::new("@@wait_timeout", ColumnType::LongLong),
                ],
                rows: vec![vec![Value::Int(64 << 20), Value::Int(28800)]],
                warnings: 0,
            })),
            _ => Err(SqlError::not_supported(sql)),
        }
    }

    async fn prepare(&mut self, stmt_id: u32, sql: &str) -> Result<PreparedInfo, SqlError> {
        let lower = sql.trim().to_ascii_lowercase();
        let param_count = lower.matches('?').count() as u16;
        if !lower.starts_with("select") {
            return Err(SqlError::not_supported(sql));
        }
        self.stmts.insert(stmt_id, lower);
        Ok(PreparedInfo {
            param_count,
            columns: vec![],
        })
    }

    async fn execute(&mut self, stmt_id: u32, params: Vec<Param>) -> Result<Response, SqlError> {
        let sql = self
            .stmts
            .get(&stmt_id)
            .ok_or_else(|| SqlError::unknown_stmt(stmt_id, "execute"))?;
        if sql == "select types" {
            return Ok(Response::ResultSet(typed_result()));
        }
        // Echo the parameters, one column each, typed after the value.
        let columns = params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let ty = match &p.value {
                    Value::Null => ColumnType::Null,
                    Value::Int(_) | Value::UInt(_) => ColumnType::LongLong,
                    Value::Float(_) => ColumnType::Float,
                    Value::Double(_) => ColumnType::Double,
                    Value::Decimal(_) => ColumnType::NewDecimal,
                    Value::Bytes(_) => ColumnType::VarString,
                    Value::DateTime(_) => ColumnType::Datetime,
                    Value::Time(_) => ColumnType::Duration,
                };
                let mut c = ColumnDef::new(format!("p{i}"), ty);
                if matches!(p.value, Value::UInt(_)) {
                    c.flags |= tidb_mysql_protocol::constants::column_flag::UNSIGNED;
                }
                c
            })
            .collect();
        Ok(Response::ResultSet(ResultSet {
            columns,
            rows: vec![params.into_iter().map(|p| p.value).collect()],
            warnings: 0,
        }))
    }

    fn close(&mut self, stmt_id: u32) {
        self.stmts.remove(&stmt_id);
    }

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        if db == "missing" {
            return Err(SqlError::bad_db(db));
        }
        self.db = Some(db.to_string());
        Ok(())
    }
}

async fn start(config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::new(config, Factory)
            .serve(listener, std::future::pending())
            .await
            .unwrap();
    });
    addr
}

fn opts(addr: SocketAddr, user: &str, pass: Option<&str>, db: Option<&str>) -> Opts {
    OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some(user))
        .pass(pass)
        .db_name(db)
        .prefer_socket(false)
        .into()
}

fn sha2_config() -> ServerConfig {
    ServerConfig {
        default_auth_plugin: auth_plugin::CACHING_SHA2_PASSWORD.into(),
        ..ServerConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn text_query_and_ping() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let v: Option<i64> = conn.query_first("select 1").await.unwrap();
    assert_eq!(v, Some(1));
    let n: Vec<i64> = conn.query("select many").await.unwrap();
    assert_eq!(n, (0..1000).collect::<Vec<_>>());
    conn.ping().await.unwrap();
    assert_eq!(conn.server_version(), (8, 0, 11));
    conn.disconnect().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn text_protocol_types() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let row: Row = conn.query_first("select types").await.unwrap().unwrap();
    check_typed_row(row);
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_protocol_types() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let row: Row = conn.exec_first("select types", ()).await.unwrap().unwrap();
    check_typed_row(row);
}

fn check_typed_row(row: Row) {
    let i: i64 = row.get(0).unwrap();
    let t: i64 = row.get(1).unwrap();
    let d: f64 = row.get(2).unwrap();
    let s: String = row.get(3).unwrap();
    let dec: String = row.get(4).unwrap();
    let dt: String = row.get::<mysql_async::Value, _>(5).unwrap().as_sql(true);
    let day: String = row.get::<mysql_async::Value, _>(6).unwrap().as_sql(true);
    let tm = row.get::<mysql_async::Value, _>(7).unwrap();
    let n: Option<i64> = row.get(8).unwrap();
    assert_eq!(
        (i, t, d, s.as_str(), dec.as_str()),
        (-42, 7, 2.5, "héllo", "12.34")
    );
    assert_eq!(dt, "'2024-02-29 13:14:15.123456'");
    assert!(
        day == "'1999-12-31'" || day == "'1999-12-31 00:00:00'",
        "{day}"
    );
    match tm {
        // Text protocol: the TIME as text.
        mysql_async::Value::Bytes(b) => assert_eq!(b, b"-25:01:02"),
        // Binary protocol: negative, 1 day, 1 hour, 1 minute, 2 seconds.
        mysql_async::Value::Time(neg, days, h, m, s, us) => {
            assert_eq!((neg, days, h, m, s, us), (true, 1, 1, 1, 2, 0))
        }
        other => panic!("unexpected TIME {other:?}"),
    }
    assert_eq!(n, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_statement_params() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let v: Option<i64> = conn.exec_first("SELECT ?", (42,)).await.unwrap();
    assert_eq!(v, Some(42));
    let v: Option<String> = conn.exec_first("SELECT ?", ("hello",)).await.unwrap();
    assert_eq!(v.as_deref(), Some("hello"));
    let v: Option<f64> = conn.exec_first("SELECT ?", (1.25f64,)).await.unwrap();
    assert_eq!(v, Some(1.25));
    let v: Option<u64> = conn.exec_first("SELECT ?", (u64::MAX,)).await.unwrap();
    assert_eq!(v, Some(u64::MAX));
    let v: Option<(Option<i64>, String, i64)> = conn
        .exec_first("SELECT ?, ?, ?", (None::<i64>, "x", -3))
        .await
        .unwrap();
    assert_eq!(v, Some((None, "x".to_string(), -3)));
    // A reused prepared statement: later executes may omit the parameter types.
    let stmt = conn.prep("SELECT ?").await.unwrap();
    for i in 0..20i64 {
        let v: Option<i64> = conn.exec_first(&stmt, (i,)).await.unwrap();
        assert_eq!(v, Some(i));
    }
    conn.close(stmt).await.unwrap();
    // Prepare errors come back as SQL errors and the connection stays usable.
    let err = conn.prep("DELETE FROM t").await.unwrap_err();
    match err {
        mysql_async::Error::Server(e) => assert_eq!((e.code, e.state.as_str()), (1235, "42000")),
        other => panic!("unexpected {other:?}"),
    }
    let v: Option<i64> = conn.query_first("select 1").await.unwrap();
    assert_eq!(v, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn large_result_packet_is_split() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let v: Option<Vec<u8>> = conn.query_first("select big").await.unwrap();
    let v = v.unwrap();
    assert_eq!(v.len(), (1 << 24) + 100);
    assert!(v.iter().all(|&b| b == b'x'));
}

#[tokio::test(flavor = "multi_thread")]
async fn native_password_auth() {
    let addr = start(ServerConfig::default()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "alice", Some(PASSWORD), Some("db1")))
        .await
        .unwrap();
    let v: Option<String> = conn.query_first("select database()").await.unwrap();
    assert_eq!(v.as_deref(), Some("db1"));
    conn.disconnect().await.unwrap();

    for (user, pass) in [
        ("alice", Some("wrong")),
        ("alice", None),
        ("bob", Some(PASSWORD)),
    ] {
        match mysql_async::Conn::new(opts(addr, user, pass, None)).await {
            Err(mysql_async::Error::Server(e)) => {
                assert_eq!((e.code, e.state.as_str()), (1045, "28000"));
                let using = if pass.is_some() { "YES" } else { "NO" };
                assert_eq!(
                    e.message,
                    format!(
                        "Access denied for user '{user}'@'127.0.0.1' (using password: {using})"
                    )
                );
            }
            other => panic!(
                "expected access denied for {user}, got {:?}",
                other.map(|_| ())
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn caching_sha2_client_is_switched_to_native() {
    // The server advertises caching_sha2_password, so the client answers with a SHA-256
    // scramble; the server must switch it to mysql_native_password.
    let addr = start(sha2_config()).await;
    let mut conn = mysql_async::Conn::new(opts(addr, "alice", Some(PASSWORD), None))
        .await
        .unwrap();
    let v: Option<i64> = conn.query_first("select 1").await.unwrap();
    assert_eq!(v, Some(1));
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let v: Option<i64> = conn.exec_first("SELECT ?", (5,)).await.unwrap();
    assert_eq!(v, Some(5));
    let err = mysql_async::Conn::new(opts(addr, "alice", Some("nope"), None))
        .await
        .unwrap_err();
    assert!(
        matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_default_db_and_init_db() {
    let addr = start(ServerConfig::default()).await;
    let err = mysql_async::Conn::new(opts(addr, "root", None, Some("missing")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, mysql_async::Error::Server(ref e) if e.code == 1049),
        "{err:?}"
    );
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    conn.reset().await.unwrap();
    let v: Option<i64> = conn.query_first("select 1").await.unwrap();
    assert_eq!(v, Some(1));
}

// ---------------------------------------------------------------------------------------------
// Minimal raw client, to check exact packet sequences with and without CLIENT_DEPRECATE_EOF.

struct Raw {
    io: PacketIo<TcpStream>,
}

impl Raw {
    async fn connect(
        addr: SocketAddr,
        caps: u32,
        user: &str,
        password: &str,
        plugin: &str,
    ) -> (Self, Vec<u8>) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut io = PacketIo::new(stream, 1 << 30);
        let hs = io.read_packet().await.unwrap();
        assert_eq!(hs[0], 10);
        // Salt: 8 bytes after version + conn id, 12 more after the reserved bytes.
        let ver_end = hs.iter().position(|&b| b == 0).unwrap();
        let p = ver_end + 1 + 4;
        let mut salt = hs[p..p + 8].to_vec();
        let p2 = p + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
        salt.extend_from_slice(&hs[p2..p2 + 12]);
        let scramble =
            tidb_mysql_protocol::auth::native_password_scramble(password.as_bytes(), &salt);
        let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
            caps, user, &scramble, None, plugin,
        );
        io.write_packet(&resp).await.unwrap();
        io.flush().await.unwrap();
        let reply = io.read_packet().await.unwrap();
        (Raw { io }, reply)
    }

    async fn command(&mut self, cmd: u8, body: &[u8]) {
        self.io.reset_sequence();
        let mut p = vec![cmd];
        p.extend_from_slice(body);
        self.io.write_packet(&p).await.unwrap();
        self.io.flush().await.unwrap();
    }

    async fn read(&mut self) -> Vec<u8> {
        self.io.read_packet().await.unwrap()
    }
}

const BASE_CAPS: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_LONG_PASSWORD;

fn is_eof(p: &[u8]) -> bool {
    p[0] == 0xfe && p.len() == 5
}

fn is_ok_eof(p: &[u8]) -> bool {
    p[0] == 0xfe && p.len() == 7
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_without_deprecate_eof() {
    let addr = start(ServerConfig::default()).await;
    let (mut c, ok) = Raw::connect(
        addr,
        BASE_CAPS,
        "alice",
        PASSWORD,
        auth_plugin::MYSQL_NATIVE_PASSWORD,
    )
    .await;
    // OK, affected 0, insert id 0, status AUTOCOMMIT, warnings 0.
    assert_eq!(ok, [0, 0, 0, 2, 0, 0, 0]);

    c.command(command::QUERY, b"select 1").await;
    assert_eq!(c.read().await, [1]); // column count
    let col = c.read().await;
    assert_eq!(&col[..4], b"\x03def");
    let eof = c.read().await;
    assert!(is_eof(&eof), "{eof:?}");
    assert_eq!(&eof[3..5], &status::AUTOCOMMIT.to_le_bytes());
    assert_eq!(c.read().await, [1, b'1']);
    assert!(is_eof(&c.read().await));

    // Prepare "SELECT ?, ?": stmt ok, 2 param defs + EOF, no columns.
    c.command(command::STMT_PREPARE, b"SELECT ?, ?").await;
    let ok = c.read().await;
    assert_eq!(ok[0], 0);
    let stmt_id = u32::from_le_bytes([ok[1], ok[2], ok[3], ok[4]]);
    assert_eq!(&ok[5..], &[0, 0, 2, 0, 0, 0, 0]);
    for _ in 0..2 {
        let def = c.read().await;
        assert_eq!(def[def.len() - 6], ColumnType::Blob as u8);
    }
    assert!(is_eof(&c.read().await));

    // Execute with a NULL and a LONGLONG.
    let mut body = stmt_id.to_le_bytes().to_vec();
    body.push(0); // flags
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0b01); // null bitmap: param 0 is NULL
    body.push(1); // new params bound
    body.extend_from_slice(&[ColumnType::Null as u8, 0, ColumnType::LongLong as u8, 0]);
    body.extend_from_slice(&99i64.to_le_bytes());
    c.command(command::STMT_EXECUTE, &body).await;
    assert_eq!(c.read().await, [2]);
    c.read().await;
    c.read().await;
    assert!(is_eof(&c.read().await));
    let row = c.read().await;
    // 0x00 header, 1 bitmap byte with column 0 -> bit 2, then 99 as 8 bytes.
    let mut expected = vec![0, 0b100];
    expected.extend_from_slice(&99i64.to_le_bytes());
    assert_eq!(row, expected);
    assert!(is_eof(&c.read().await));

    // Re-execute without types (new-params-bound = 0) and with long data for param 1.
    let mut ld = stmt_id.to_le_bytes().to_vec();
    ld.extend_from_slice(&1u16.to_le_bytes());
    ld.extend_from_slice(b"long");
    c.command(command::STMT_SEND_LONG_DATA, &ld).await; // no response
    let mut body = stmt_id.to_le_bytes().to_vec();
    body.push(0);
    body.extend_from_slice(&1u32.to_le_bytes());
    body.push(0b01);
    body.push(0);
    c.command(command::STMT_EXECUTE, &body).await;
    assert_eq!(c.read().await, [2]);
    c.read().await;
    let col1 = c.read().await;
    assert_eq!(col1[col1.len() - 6], ColumnType::VarString as u8);
    assert!(is_eof(&c.read().await));
    assert_eq!(c.read().await, [0, 0b100, 4, b'l', b'o', b'n', b'g']);
    assert!(is_eof(&c.read().await));

    // Unknown statement -> ERR 1243; unknown command -> ERR 1105.
    c.command(command::STMT_RESET, &999u32.to_le_bytes()).await;
    let err = c.read().await;
    assert_eq!(err[0], 0xff);
    assert_eq!(u16::from_le_bytes([err[1], err[2]]), 1243);
    c.command(command::SLEEP, &[]).await;
    let err = c.read().await;
    assert_eq!(&err[..9], b"\xff\x51\x04#HY000");
    assert_eq!(&err[9..], b"command 0 not supported now");

    c.command(command::STMT_CLOSE, &stmt_id.to_le_bytes()).await; // no response
    c.command(command::PING, &[]).await;
    assert_eq!(c.read().await, [0, 0, 0, 2, 0, 0, 0]);
    c.command(command::QUIT, &[]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_with_deprecate_eof() {
    let addr = start(ServerConfig::default()).await;
    let caps = BASE_CAPS | CLIENT_DEPRECATE_EOF;
    let (mut c, ok) =
        Raw::connect(addr, caps, "root", "", auth_plugin::MYSQL_NATIVE_PASSWORD).await;
    assert_eq!(ok[0], 0);

    c.command(command::QUERY, b"select 1").await;
    assert_eq!(c.read().await, [1]);
    c.read().await; // column def, no EOF after it
    assert_eq!(c.read().await, [1, b'1']);
    let end = c.read().await;
    assert!(is_ok_eof(&end), "{end:?}");
    assert_eq!(end, [0xfe, 0, 0, 2, 0, 0, 0]);

    c.command(command::STMT_PREPARE, b"SELECT ?").await;
    let ok = c.read().await;
    assert_eq!(&ok[5..9], &[0, 0, 1, 0]);
    c.read().await; // param def, no EOF
    // The next packet must be the response to the next command.
    c.command(command::PING, &[]).await;
    assert_eq!(c.read().await, [0, 0, 0, 2, 0, 0, 0]);

    // Errors carry SQLSTATE.
    c.command(command::QUERY, b"select nope").await;
    let err = c.read().await;
    assert_eq!(&err[..9], b"\xff\xd3\x04#42000");
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_auth_switch_from_caching_sha2() {
    let addr = start(sha2_config()).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut io = PacketIo::new(stream, 1 << 30);
    let hs = io.read_packet().await.unwrap();
    assert!(hs.ends_with(b"caching_sha2_password\0"));
    let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
        BASE_CAPS,
        "alice",
        &[0xaa; 32],
        None,
        auth_plugin::CACHING_SHA2_PASSWORD,
    );
    io.write_packet(&resp).await.unwrap();
    io.flush().await.unwrap();
    let switch = io.read_packet().await.unwrap();
    assert_eq!(io.sequence(), 3);
    assert_eq!(switch[0], 0xfe);
    assert_eq!(&switch[1..23], b"mysql_native_password\0");
    let salt = &switch[23..43];
    let scramble = tidb_mysql_protocol::auth::native_password_scramble(PASSWORD.as_bytes(), salt);
    io.write_packet(&scramble).await.unwrap();
    io.flush().await.unwrap();
    let ok = io.read_packet().await.unwrap();
    assert_eq!(ok, [0, 0, 0, 2, 0, 0, 0]);
    assert_eq!(io.sequence(), 5);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_disconnect_mid_session_is_harmless() {
    let addr = start(ServerConfig::default()).await;
    for _ in 0..20 {
        let (c, _) = Raw::connect(
            addr,
            BASE_CAPS,
            "root",
            "",
            auth_plugin::MYSQL_NATIVE_PASSWORD,
        )
        .await;
        drop(c);
        // Truncated packet header, then hang up.
        let mut s = TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut s, &[1, 0])
            .await
            .unwrap();
        drop(s);
    }
    let mut conn = mysql_async::Conn::new(opts(addr, "root", None, None))
        .await
        .unwrap();
    let v: Option<i64> = conn.query_first("select 1").await.unwrap();
    assert_eq!(v, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_native_client_with_sha2_server_is_switched() {
    // `mysql --default-auth=mysql_native_password` against a server advertising
    // caching_sha2_password sends the native plugin name with empty auth data; the server must
    // send an AuthSwitchRequest (TiDB: cc.authPlugin != resp.AuthPlugin).
    let addr = start(sha2_config()).await;
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut io = PacketIo::new(stream, 1 << 30);
    io.read_packet().await.unwrap();
    let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
        BASE_CAPS,
        "alice",
        &[],
        None,
        auth_plugin::MYSQL_NATIVE_PASSWORD,
    );
    io.write_packet(&resp).await.unwrap();
    io.flush().await.unwrap();
    let switch = io.read_packet().await.unwrap();
    assert_eq!(&switch[..23], b"\xfemysql_native_password\0");
    let scramble =
        tidb_mysql_protocol::auth::native_password_scramble(PASSWORD.as_bytes(), &switch[23..43]);
    io.write_packet(&scramble).await.unwrap();
    io.flush().await.unwrap();
    assert_eq!(io.read_packet().await.unwrap(), [0, 0, 0, 2, 0, 0, 0]);
}
