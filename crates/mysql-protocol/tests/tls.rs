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
    ColumnDef, ColumnType, ConnectionInfo, Credential, Param, PreparedInfo, Response, ResultSet,
    Server, ServerConfig, SessionFactory, SessionHandler, SqlError, Value,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls;

const PASSWORD: &str = "tls-pw";

struct Factory;

impl SessionFactory for Factory {
    type Session = Session;

    async fn lookup_user(&self, user: &str, _host: &str) -> Option<Credential> {
        (user == "alice").then(|| Credential::native_from_password(PASSWORD))
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
