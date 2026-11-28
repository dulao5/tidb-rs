//! TLS end-to-end tests: SSLRequest -> TLS upgrade -> handshake response over TLS.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, SslOpts};
use tidb_mysql_protocol::constants::{auth_plugin, capability::*, command};
use tidb_mysql_protocol::packet::PacketIo;
use tidb_mysql_protocol::tls::{generate_self_signed, tls_config_from_pem};
use tidb_mysql_protocol::{
    AccountAuth, ColumnDef, ColumnType, ConnectionInfo, Credential, Param, PreparedInfo, Response,
    ResultSet, Server, ServerConfig, SessionFactory, SessionHandler, SqlError, Value,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;

const PASSWORD: &str = "tls-pw";

struct Factory;

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        self.lookup_account(user, _host).await.map(|a| a.credential)
    }

    async fn lookup_account(&self, user: &str, _host: &str) -> Option<AccountAuth> {
        match user {
            "alice" => Some(AccountAuth::from(Credential::native_from_password(
                PASSWORD,
            ))),
            "bob" => Some(AccountAuth::from(Credential::caching_sha2_from_password(
                PASSWORD,
            ))),
            "bob_nopass" => Some(AccountAuth::from(Credential::caching_sha2_from_password(
                "",
            ))),
            "bob_locked" => Some(AccountAuth {
                credential: Credential::caching_sha2_from_password(PASSWORD),
                auth_user: Some("bob_locked".to_owned()),
                auth_host: Some("%".to_owned()),
                account_locked: true,
                password_expired: false,
            }),
            "bob_expired" => Some(AccountAuth {
                credential: Credential::caching_sha2_from_password(PASSWORD),
                auth_user: Some("bob_expired".to_owned()),
                auth_host: Some("%".to_owned()),
                account_locked: false,
                password_expired: true,
            }),
            _ => None,
        }
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<Session, SqlError> {
        Ok(Session {
            cipher: info.tls_cipher.clone(),
        })
    }
}

struct Session {
    cipher: Option<String>,
}

fn one(name: &str, ty: ColumnType, v: Value) -> Response {
    Response::ResultSet(ResultSet {
        columns: vec![ColumnDef::new(name, ty)],
        rows: vec![vec![v]],
        warnings: 0,
    })
}

impl SessionHandler for Session {
    async fn query(&mut self, sql: &str) -> Result<Response, SqlError> {
        match sql {
            "SELECT @@max_allowed_packet,@@wait_timeout" => Ok(Response::ResultSet(ResultSet {
                columns: vec![
                    ColumnDef::new("@@max_allowed_packet", ColumnType::LongLong),
                    ColumnDef::new("@@wait_timeout", ColumnType::LongLong),
                ],
                rows: vec![vec![Value::Int(64 << 20), Value::Int(28800)]],
                warnings: 0,
            })),
            "select 1" => Ok(one("1", ColumnType::LongLong, Value::Int(1))),
            "select cipher" => Ok(one(
                "cipher",
                ColumnType::VarString,
                self.cipher.clone().map_or(Value::Null, Value::from),
            )),
            _ => Err(SqlError::not_supported(sql)),
        }
    }

    async fn prepare(&mut self, _stmt_id: u32, _sql: &str) -> Result<PreparedInfo, SqlError> {
        Ok(PreparedInfo {
            param_count: 1,
            columns: vec![],
            ..Default::default()
        })
    }

    async fn execute(&mut self, _stmt_id: u32, params: Vec<Param>) -> Result<Response, SqlError> {
        Ok(one(
            "?",
            ColumnType::LongLong,
            params.into_iter().next().map_or(Value::Null, |p| p.value),
        ))
    }

    fn close(&mut self, _stmt_id: u32) {}

    async fn use_db(&mut self, _db: &str) -> Result<(), SqlError> {
        Ok(())
    }
}

/// Starts a TLS-enabled server; returns its address and certificate (PEM).
async fn start_tls() -> (SocketAddr, String) {
    let generated = generate_self_signed(&["localhost"]).unwrap();
    let tls =
        tls_config_from_pem(generated.cert_pem.as_bytes(), generated.key_pem.as_bytes()).unwrap();
    let config = ServerConfig {
        tls: Some(tls),
        ..ServerConfig::default()
    };
    (start(config).await, generated.cert_pem)
}

async fn start(config: ServerConfig) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::new(config, Factory)
            .serve(listener, std::future::pending())
            .await
            .unwrap()
    });
    addr
}

fn opts(addr: SocketAddr, ssl: Option<SslOpts>) -> OptsBuilder {
    OptsBuilder::default()
        .ip_or_hostname(addr.ip().to_string())
        .tcp_port(addr.port())
        .user(Some("alice"))
        .pass(Some(PASSWORD))
        .prefer_socket(false)
        .ssl_opts(ssl)
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_async_over_tls() {
    let (addr, _) = start_tls().await;
    let ssl = SslOpts::default().with_danger_accept_invalid_certs(true);
    let mut conn = mysql_async::Conn::new(opts(addr, Some(ssl))).await.unwrap();
    let cipher: Option<String> = conn.query_first("select cipher").await.unwrap();
    let cipher = cipher.expect("connection should be TLS");
    assert!(
        cipher.starts_with("TLS13_") || cipher.starts_with("TLS_ECDHE"),
        "{cipher}"
    );
    for i in 0..10i64 {
        let v: Option<i64> = conn.exec_first("SELECT ?", (i,)).await.unwrap();
        assert_eq!(v, Some(i));
    }
    conn.disconnect().await.unwrap();

    // Plain connections keep working on a TLS-enabled server.
    let mut conn = mysql_async::Conn::new(opts(addr, None)).await.unwrap();
    let cipher: Option<Option<String>> = conn.query_first("select cipher").await.unwrap();
    assert_eq!(cipher, Some(None));
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_async_wrong_password_over_tls() {
    let (addr, _) = start_tls().await;
    let ssl = SslOpts::default().with_danger_accept_invalid_certs(true);
    let err = mysql_async::Conn::new(opts(addr, Some(ssl)).pass(Some("nope")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
        "{err:?}"
    );
}

/// Client stream that holds back `held` (the SSLRequest packet) and sends it in the same
/// write as the first TLS record (the ClientHello), to exercise the server's handling of
/// TLS bytes already buffered behind the SSLRequest.
struct Coalesce {
    held: Option<Vec<u8>>,
    pending: Vec<u8>,
    caller_len: usize,
    inner: TcpStream,
}

impl AsyncRead for Coalesce {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Coalesce {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        if let Some(mut held) = this.held.take() {
            held.extend_from_slice(buf);
            this.pending = held;
            this.caller_len = buf.len();
        }
        while !this.pending.is_empty() {
            let n = ready!(Pin::new(&mut this.inner).poll_write(cx, &this.pending))?;
            this.pending.drain(..n);
        }
        if this.caller_len > 0 {
            return Poll::Ready(Ok(std::mem::take(&mut this.caller_len)));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn client_config(cert_pem: &str) -> Arc<rustls::ClientConfig> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(cert_pem.as_bytes()).unwrap())
        .unwrap();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    Arc::new(config)
}

/// Reads the initial handshake; returns (capability flags, salt).
async fn read_handshake(stream: &mut TcpStream) -> (u32, Vec<u8>) {
    let mut io = PacketIo::new(stream, 1 << 20);
    let hs = io.read_packet().await.unwrap();
    let p = hs.iter().position(|&b| b == 0).unwrap() + 1 + 4;
    let mut salt = hs[p..p + 8].to_vec();
    let caps_lo = u16::from_le_bytes([hs[p + 9], hs[p + 10]]);
    let caps_hi = u16::from_le_bytes([hs[p + 14], hs[p + 15]]);
    let p2 = p + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
    salt.extend_from_slice(&hs[p2..p2 + 12]);
    (u32::from(caps_lo) | (u32::from(caps_hi) << 16), salt)
}

fn ssl_request(caps: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&caps.to_le_bytes());
    payload.extend_from_slice(&(16u32 << 20).to_le_bytes());
    payload.push(46);
    payload.extend_from_slice(&[0; 23]);
    // Header: 32-byte payload, sequence id 1.
    let mut packet = vec![32, 0, 0, 1];
    packet.extend_from_slice(&payload);
    packet
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_ssl_request_upgrade() {
    for coalesce in [false, true] {
        let (addr, cert_pem) = start_tls().await;
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let (server_caps, salt) = read_handshake(&mut tcp).await;
        assert_ne!(server_caps & CLIENT_SSL, 0);

        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_SSL
            | CLIENT_DEPRECATE_EOF;
        let request = ssl_request(caps);
        let stream = if coalesce {
            Coalesce {
                held: Some(request),
                pending: Vec::new(),
                caller_len: 0,
                inner: tcp,
            }
        } else {
            tokio::io::AsyncWriteExt::write_all(&mut tcp, &request)
                .await
                .unwrap();
            Coalesce {
                held: None,
                pending: Vec::new(),
                caller_len: 0,
                inner: tcp,
            }
        };
        let connector = tokio_rustls::TlsConnector::from(client_config(&cert_pem));
        let tls = connector
            .connect("localhost".try_into().unwrap(), stream)
            .await
            .unwrap();

        // The handshake response continues the sequence over TLS: 2, then the server's OK is 3.
        let mut io = PacketIo::new(tls, 1 << 20);
        io.set_sequence(2);
        let scramble =
            tidb_mysql_protocol::auth::native_password_scramble(PASSWORD.as_bytes(), &salt);
        let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
            caps,
            "alice",
            &scramble,
            None,
            auth_plugin::MYSQL_NATIVE_PASSWORD,
        );
        io.write_packet(&resp).await.unwrap();
        io.flush().await.unwrap();
        assert_eq!(io.read_packet().await.unwrap(), [0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(io.sequence(), 4);

        io.reset_sequence();
        io.write_packet(&[&[command::QUERY][..], b"select cipher"].concat())
            .await
            .unwrap();
        io.flush().await.unwrap();
        assert_eq!(io.read_packet().await.unwrap(), [1]);
        io.read_packet().await.unwrap();
        let row = io.read_packet().await.unwrap();
        assert!(row.len() > 5 && row[1..].starts_with(b"TLS"), "{row:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ssl_request_without_tls_is_rejected() {
    let addr = start(ServerConfig::default()).await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let (server_caps, _) = read_handshake(&mut tcp).await;
    assert_eq!(server_caps & CLIENT_SSL, 0);
    tokio::io::AsyncWriteExt::write_all(&mut tcp, &ssl_request(CLIENT_PROTOCOL_41 | CLIENT_SSL))
        .await
        .unwrap();
    let mut io = PacketIo::new(tcp, 1 << 20);
    io.set_sequence(2);
    let err = io.read_packet().await.unwrap();
    assert_eq!(err[0], 0xff);
    assert_eq!(u16::from_le_bytes([err[1], err[2]]), 1235);
}

#[tokio::test(flavor = "multi_thread")]
async fn caching_sha2_password_over_tls_and_plain_tcp() {
    for default_plugin in [
        auth_plugin::MYSQL_NATIVE_PASSWORD,
        auth_plugin::CACHING_SHA2_PASSWORD,
    ] {
        let generated = generate_self_signed(&["localhost"]).unwrap();
        let tls = tls_config_from_pem(generated.cert_pem.as_bytes(), generated.key_pem.as_bytes())
            .unwrap();
        let config = ServerConfig {
            tls: Some(tls),
            default_auth_plugin: default_plugin.to_owned(),
            ..ServerConfig::default()
        };
        let addr = start(config).await;
        let ssl = SslOpts::default().with_danger_accept_invalid_certs(true);

        // 1. Right password over TLS -> succeeds (exercises AuthSwitchRequest when default is
        //    mysql_native_password and direct AuthMoreData [0x01, 0x04] when default is caching_sha2).
        let mut conn = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob"))
                .pass(Some(PASSWORD))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_or_else(|e| panic!("bob TLS login failed (default={default_plugin}): {e}"));
        let one: Option<i64> = conn.query_first("select 1").await.unwrap();
        assert_eq!(one, Some(1));
        conn.disconnect().await.unwrap();

        // 2. Wrong password over TLS -> 1045.
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob"))
                .pass(Some("wrong-pw"))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
            "default={default_plugin}: {err:?}"
        );

        // 3. Empty password against non-empty caching_sha2 account over TLS -> 1045 (using password: NO).
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob"))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045 && e.message.contains("using password: NO")),
            "default={default_plugin}: {err:?}"
        );

        // 4. Locked caching_sha2 account: right password -> 3118, wrong password -> 1045.
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob_locked"))
                .pass(Some(PASSWORD))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 3118),
            "locked right pw (default={default_plugin}): {err:?}"
        );
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob_locked"))
                .pass(Some("wrong-pw"))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
            "locked wrong pw (default={default_plugin}): {err:?}"
        );

        // 4b. Expired caching_sha2 account over TLS: right password -> 1862, wrong password -> 1045.
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob_expired"))
                .pass(Some(PASSWORD))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1862),
            "expired right pw (default={default_plugin}): {err:?}"
        );
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob_expired"))
                .pass(Some("wrong-pw"))
                .prefer_socket(false)
                .ssl_opts(Some(ssl.clone())),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
            "expired wrong pw (default={default_plugin}): {err:?}"
        );

        // 5. Passwordless caching_sha2 account succeeds over both TLS and plain TCP, and rejects non-empty password.
        for use_ssl in [Some(ssl.clone()), None] {
            let mut conn = mysql_async::Conn::new(
                OptsBuilder::default()
                    .ip_or_hostname(addr.ip().to_string())
                    .tcp_port(addr.port())
                    .user(Some("bob_nopass"))
                    .prefer_socket(false)
                    .ssl_opts(use_ssl),
            )
            .await
            .unwrap();
            let one: Option<i64> = conn.query_first("select 1").await.unwrap();
            assert_eq!(one, Some(1));
            conn.disconnect().await.unwrap();
        }
        let err = mysql_async::Conn::new(
            OptsBuilder::default()
                .ip_or_hostname(addr.ip().to_string())
                .tcp_port(addr.port())
                .user(Some("bob_nopass"))
                .pass(Some("unexpected"))
                .prefer_socket(false)
                .ssl_opts(Some(ssl)),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, mysql_async::Error::Server(ref e) if e.code == 1045),
            "nopass with wrong pw (default={default_plugin}): {err:?}"
        );

        // 6. Plain TCP (no TLS) must NEVER accept cleartext password or RSA pubkey request (0x02).
        for full_auth_payload in [format!("{PASSWORD}\0").into_bytes(), vec![0x02]] {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            let _ = read_handshake(&mut tcp).await;
            let mut io = PacketIo::new(tcp, 1 << 20);
            io.set_sequence(1);
            let caps = CLIENT_PROTOCOL_41
                | CLIENT_SECURE_CONNECTION
                | CLIENT_PLUGIN_AUTH
                | CLIENT_LONG_PASSWORD;
            let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
                caps,
                "bob",
                &[0x11; 32],
                None,
                auth_plugin::CACHING_SHA2_PASSWORD,
            );
            io.write_packet(&resp).await.unwrap();
            io.flush().await.unwrap();
            let mut pkt = io.read_packet().await.unwrap();
            if pkt[0] == 0xfe {
                io.write_packet(&[0x11; 32]).await.unwrap();
                io.flush().await.unwrap();
                pkt = io.read_packet().await.unwrap();
            }
            assert_eq!(pkt, [0x01, 0x04], "expected AuthMoreData full auth request");
            io.write_packet(&full_auth_payload).await.unwrap();
            io.flush().await.unwrap();
            let final_pkt = io.read_packet().await.unwrap();
            assert_eq!(
                final_pkt[0], 0xff,
                "plain TCP must reject caching_sha2 full auth"
            );
            assert_eq!(u16::from_le_bytes([final_pkt[1], final_pkt[2]]), 1045);
        }

        // 7. libmysqlclient (MySQL 8.0 CLI) sends a single NUL byte `[0x00]` for empty passwords
        //    under caching_sha2_password: bob_nopass must succeed (OK) and bob must fail with
        //    1045 `(using password: NO)` without triggering AuthMoreData.
        for (user, expect_ok) in [("bob_nopass", true), ("bob", false)] {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            let _ = read_handshake(&mut tcp).await;
            let mut io = PacketIo::new(tcp, 1 << 20);
            io.set_sequence(1);
            let caps = CLIENT_PROTOCOL_41
                | CLIENT_SECURE_CONNECTION
                | CLIENT_PLUGIN_AUTH
                | CLIENT_LONG_PASSWORD;
            let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
                caps,
                user,
                &[0x00],
                None,
                auth_plugin::CACHING_SHA2_PASSWORD,
            );
            io.write_packet(&resp).await.unwrap();
            io.flush().await.unwrap();
            let mut pkt = io.read_packet().await.unwrap();
            if pkt[0] == 0xfe {
                io.write_packet(&[0x00]).await.unwrap();
                io.flush().await.unwrap();
                pkt = io.read_packet().await.unwrap();
            }
            if expect_ok {
                assert_eq!(
                    pkt[0], 0x00,
                    "libmysqlclient [0x00] empty auth for {user} (default={default_plugin}) must succeed: {pkt:?}"
                );
            } else {
                assert_eq!(
                    pkt[0], 0xff,
                    "libmysqlclient [0x00] empty auth for {user} (default={default_plugin}) must fail immediately: {pkt:?}"
                );
                assert_eq!(u16::from_le_bytes([pkt[1], pkt[2]]), 1045);
                let msg = std::str::from_utf8(&pkt[9..]).unwrap();
                assert!(
                    msg.contains("using password: NO"),
                    "expected using password: NO, got {msg}"
                );
            }
        }

        // 8. Full-auth NUL trimming matches Go `bytes.Trim(data, "\x00")` (leading + trailing NULs).
        {
            let mut tcp = TcpStream::connect(addr).await.unwrap();
            let _ = read_handshake(&mut tcp).await;
            let caps = CLIENT_PROTOCOL_41
                | CLIENT_SECURE_CONNECTION
                | CLIENT_PLUGIN_AUTH
                | CLIENT_SSL
                | CLIENT_LONG_PASSWORD;
            tokio::io::AsyncWriteExt::write_all(&mut tcp, &ssl_request(caps))
                .await
                .unwrap();
            let connector = tokio_rustls::TlsConnector::from(client_config(&generated.cert_pem));
            let tls = connector
                .connect("localhost".try_into().unwrap(), tcp)
                .await
                .unwrap();
            let mut io = PacketIo::new(tls, 1 << 20);
            io.set_sequence(2);
            let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
                caps,
                "bob",
                &[0x11; 32],
                None,
                auth_plugin::CACHING_SHA2_PASSWORD,
            );
            io.write_packet(&resp).await.unwrap();
            io.flush().await.unwrap();
            let mut pkt = io.read_packet().await.unwrap();
            if pkt[0] == 0xfe {
                io.write_packet(&[0x11; 32]).await.unwrap();
                io.flush().await.unwrap();
                pkt = io.read_packet().await.unwrap();
            }
            assert_eq!(pkt, [0x01, 0x04]);
            let mut padded = vec![0x00, 0x00];
            padded.extend_from_slice(PASSWORD.as_bytes());
            padded.extend_from_slice(&[0x00, 0x00]);
            io.write_packet(&padded).await.unwrap();
            io.flush().await.unwrap();
            let ok = io.read_packet().await.unwrap();
            assert_eq!(
                ok[0], 0x00,
                "leading/trailing NULs must be trimmed like Go bytes.Trim (default={default_plugin}): {ok:?}"
            );
        }
    }
}
