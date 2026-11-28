//! One client connection: handshake, authentication and the command loop
//! (TiDB `server/conn.go`, `server/conn_stmt.go`).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::auth::{Credential, generate_salt};
use crate::codec::{Reader, put_lenenc_int, put_lenenc_str};
use crate::constants::{auth_plugin, capability::*, collation, command, cursor_type, header};
use crate::error::{SqlError, code};
use crate::handler::{
    ConnectionInfo, EncodedResultSet, OkInfo, RawParams, Response, ResultSet, SessionFactory,
    SessionHandler,
};
use crate::handshake::{HandshakeResponse, InitialHandshake, auth_switch_request};
use crate::packet::{PacketError, PacketIo};
use crate::params::decode_params;
use crate::server::ServerConfig;
use crate::tls::{MaybeTls, Rewind};
use crate::value::{
    ColumnDef, ColumnType, encode_binary_row, encode_column_count, encode_text_row,
};

/// Why a connection stopped or a command failed.
#[derive(Debug)]
enum ConnError {
    /// The connection is unusable (I/O error, protocol violation): close it.
    Fatal(String),
    /// Report to the client and keep the connection open.
    Sql(SqlError),
}

impl From<io::Error> for ConnError {
    fn from(e: io::Error) -> Self {
        ConnError::Fatal(e.to_string())
    }
}

impl From<SqlError> for ConnError {
    fn from(e: SqlError) -> Self {
        ConnError::Sql(e)
    }
}

/// State of a prepared statement kept by the protocol layer.
#[derive(Debug)]
struct StmtState {
    sql: String,
    param_count: u16,
    /// Parameter types from the last execute that sent them (`new-params-bound-flag`).
    param_types: Vec<u8>,
    /// Data received with `COM_STMT_SEND_LONG_DATA`, per parameter.
    long_data: Vec<Option<Vec<u8>>>,
    /// Bound parameters from the most recent `COM_STMT_EXECUTE` (used by `COM_STMT_FETCH` logging).
    last_params: Vec<crate::params::Param>,
}

/// Shared connection status inspected by [`crate::Server::serve`] during graceful drain
/// (Go `clientConn.status` + `SessionVars.InTxn()`, `server.go:1031`).
pub(crate) struct ConnStatus {
    /// True while awaiting the next client packet (`connStatusReading`).
    idle_reading: AtomicBool,
    /// True while the connection has an open explicit transaction (`SERVER_STATUS_IN_TRANS`).
    in_txn: AtomicBool,
}

impl ConnStatus {
    pub(crate) fn new() -> Self {
        Self {
            idle_reading: AtomicBool::new(false),
            in_txn: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_reading(&self, reading: bool, in_txn: bool) {
        self.in_txn.store(in_txn, Ordering::Release);
        self.idle_reading.store(reading, Ordering::Release);
    }

    /// True if graceful drain must wait for this connection (dispatching a command OR inside
    /// an explicit transaction). Idle connections outside a transaction are skipped immediately,
    /// matching Go `server.go:1031`.
    pub(crate) fn needs_drain_wait(&self) -> bool {
        !(self.idle_reading.load(Ordering::Acquire) && !self.in_txn.load(Ordering::Acquire))
    }
}

/// Runs a connection to completion. Errors are logged, never propagated.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run<S, F>(
    stream: S,
    peer: String,
    connection_id: u64,
    started: Instant,
    config: &ServerConfig,
    factory: &F,
    admission: Option<tidb_runtime::Admission>,
    status: Arc<ConnStatus>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    // Go counts the connection from accept (`newConn`) until it is closed.
    let _gauge = ConnectionGauge::new();
    let accepted = Instant::now();
    let io = PacketIo::new(MaybeTls::Plain(stream), config.max_allowed_packet);
    let mut conn = match handshake(io, &peer, connection_id, config, factory).await {
        Ok((mut io, (handler, capability, user))) => {
            io.flush_counts();
            Conn {
                io,
                capability,
                handler,
                peer: peer.clone(),
                user,
                last_sql: String::new(),
                stmts: HashMap::new(),
                next_stmt_id: 1,
                started,
                last_active: accepted,
                admission,
                token: None,
                status,
            }
        }
        Err(HandshakeError::Eof) => {
            tidb_base::log_debug!(
                "EOF",
                "conn" => connection_id,
                "remote addr" => peer.as_str(),
            );
            return;
        }
        Err(HandshakeError::Other(e)) => {
            tidb_base::log_warn!(
                "Server.onConn handshake",
                "conn" => connection_id,
                "error" => e.as_str(),
                "remote addr" => peer.as_str(),
            );
            return;
        }
    };
    tidb_base::log_debug!(
        "new connection",
        "conn" => connection_id,
        "remoteAddr" => peer.as_str(),
    );
    conn.command_loop(connection_id).await;
    for id in std::mem::take(&mut conn.stmts).into_keys() {
        conn.handler.close(id);
    }
    let _ = conn.io.shutdown().await;
    tidb_base::log_debug!("connection closed", "conn" => connection_id);
}

/// Default `QueryLogMaxLen` matching Go `executor.FormatSQL` (4096 bytes).
pub const QUERY_LOG_MAX_LEN: usize = 4096;

/// Live `tidb_query_log_max_len` read from `tidb_sessionctx::variable::tidb_vars::QueryLogMaxLen`.
pub fn current_query_log_max_len() -> usize {
    let v = tidb_sessionctx::variable::tidb_vars::QueryLogMaxLen.load(Ordering::SeqCst);
    if v <= 0 { 0 } else { v as usize }
}

/// Format SQL for `command dispatched failed` (Go `executor.FormatSQL` + `QueryReplacer`):
/// replaces `\r`, `\n`, `\t` with single spaces `' '` and truncates at `max_len` with
/// `"(len:<total>)"` (no extra space before `(len:...)`, matching Go `adapter.go:1498`).
// glue: legacy protocol-layer FormatSQL mirror (tidb-executor carries the layered FormatSQL)
pub fn format_sql_for_conn_log(sql: &str, max_len: usize) -> String {
    let mut folded = String::with_capacity(sql.len());
    for ch in sql.chars() {
        match ch {
            '\r' | '\n' | '\t' => folded.push(' '),
            c => folded.push(c),
        }
    }
    if max_len == 0 || folded.len() <= max_len {
        return folded;
    }
    let mut end = max_len;
    while end > 0 && !folded.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}(len:{})", &folded[..end], folded.len())
}

/// Format an error for the `err` field of `command dispatched failed` (Go `conn.go:1241-1257`
/// `errStrForLog`):
/// - When `redact_mode != "OFF"` and error is `ErrParse` (1064): fixed redaction message.
/// - For `ErrKeyExists` (1062), `ErrParse` (1064), or `ErrTableNotExists` (1146): message only,
///   no stack trace.
/// - For all other errors: `errors.ErrorStack(err)` (full stack regardless of
///   `disable-error-stack`).
// go: errStrForLog L1
pub fn err_str_for_log(err: &SqlError, redact_mode: &str) -> Vec<u8> {
    const ERR_KEY_EXISTS: u16 = 1062;
    const ERR_PARSE: u16 = 1064;
    const ERR_TABLE_NOT_EXISTS: u16 = 1146;

    if redact_mode != tidb_base::errors::RedactLogDisable && err.code == ERR_PARSE {
        return b"fail to parse SQL, and must redact the whole error when enable log redaction"
            .to_vec();
    }
    if matches!(err.code, ERR_KEY_EXISTS | ERR_PARSE | ERR_TABLE_NOT_EXISTS) {
        if let Some(src) = &err.source_err {
            return src.error();
        }
        return err.message.as_bytes().to_vec();
    }
    if let Some(src) = &err.source_err {
        return src.error_stack();
    }
    err.message.as_bytes().to_vec()
}

/// `tidb_server_connections` for one connection: up when created, down when dropped.
struct ConnectionGauge;

impl ConnectionGauge {
    fn new() -> Self {
        tidb_metrics::connection_opened();
        ConnectionGauge
    }
}

impl Drop for ConnectionGauge {
    fn drop(&mut self) {
        tidb_metrics::connection_closed();
    }
}

/// Writes an ERR packet (`#` + SQLSTATE only for 4.1 clients).
async fn write_err<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut PacketIo<S>,
    capability: u32,
    e: &SqlError,
) -> io::Result<()> {
    let mut b = Vec::with_capacity(16 + e.message.len());
    b.push(header::ERR);
    b.extend_from_slice(&e.code.to_le_bytes());
    if capability & CLIENT_PROTOCOL_41 != 0 {
        b.push(b'#');
        b.extend_from_slice(e.state.as_bytes());
    }
    b.extend_from_slice(e.message.as_bytes());
    io.write_packet(&b).await?;
    io.flush().await
}

/// Sends a handshake ERR packet and logs write failures like Go `conn.go:324-327`.
async fn write_handshake_err<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut PacketIo<S>,
    capability: u32,
    connection_id: u64,
    e: &SqlError,
) {
    if let Err(err1) = write_err(io, capability, e).await {
        let err_str = err1.to_string();
        tidb_base::log_debug!(
            "writeError failed",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
    }
}

/// The session, effective capability flags, and authenticated username.
type Accepted<F> = (<F as SessionFactory>::Session, u32, String);

/// Handshake failure classification matching Go `Server.onConn` (`server.go:702-712`).
enum HandshakeError {
    Eof,
    Other(String),
}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Eof
        } else {
            Self::Other(e.to_string())
        }
    }
}

impl From<PacketError> for HandshakeError {
    fn from(e: PacketError) -> Self {
        match e {
            PacketError::Eof => Self::Eof,
            other => Self::Other(other.log_string()),
        }
    }
}

fn is_packet_eof(e: &PacketError) -> bool {
    matches!(e, PacketError::Eof)
}

/// Performs the connection phase, including the TLS upgrade after an SSLRequest. Returns the
/// (possibly upgraded) packet stream and accepted session; `Err` on rejection, I/O, or TLS failure
/// (so `run` logs Go's `Server.onConn handshake` WARN or `EOF` DEBUG, matching `server.go:698-714`).
async fn handshake<S, F>(
    mut io: PacketIo<MaybeTls<S>>,
    peer: &str,
    connection_id: u64,
    config: &ServerConfig,
    factory: &F,
) -> Result<(PacketIo<MaybeTls<S>>, Accepted<F>), HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    let outcome = handshake_inner(&mut io, peer, connection_id, config, factory).await?;
    match outcome {
        Outcome::Done(accepted) => Ok((io, accepted)),
        Outcome::Upgrade { salt, seq } => {
            // Switch to TLS. Bytes the client sent right behind the SSLRequest (its
            // ClientHello) may already sit in the read buffer: replay them to rustls.
            let Some(tls) = &config.tls else {
                return Err(HandshakeError::Other(
                    "TLS upgrade without TLS config".into(),
                ));
            };
            let (stream, buffered) = io.into_parts();
            let MaybeTls::Plain(stream) = stream else {
                return Err(HandshakeError::Other("TLS requested twice".into()));
            };
            let acceptor = tokio_rustls::TlsAcceptor::from(tls.clone());
            let tls_stream = acceptor
                .accept(Rewind::new(buffered, stream))
                .await
                .map_err(|e| {
                    // Go pkg/server/conn.go:2584-2586: return raw TLS error (no invented prefix);
                    // client hangup mid-TLS-handshake routes to DEBUG "EOF".
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        HandshakeError::Eof
                    } else {
                        HandshakeError::Other(e.to_string())
                    }
                })?;
            let mut io = PacketIo::new(
                MaybeTls::Tls(Box::new(tls_stream)),
                config.max_allowed_packet,
            );
            io.set_sequence(seq);
            let data = io.read_packet().await.map_err(|e| {
                // Go pkg/server/conn.go:613-616: WARN for any read error after TLS upgrade.
                let err_str = e.log_string();
                tidb_base::log_warn!(
                    "read handshake response failure after upgrade to TLS",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                HandshakeError::from(e)
            })?;
            let accepted =
                authenticate(&mut io, &data, &salt, peer, connection_id, config, factory).await?;
            Ok((io, accepted))
        }
    }
}

enum Outcome<F: SessionFactory> {
    Done(Accepted<F>),
    /// The client sent an SSLRequest; continue over TLS with this salt and sequence id.
    Upgrade {
        salt: [u8; crate::auth::SALT_LEN],
        seq: u8,
    },
}

/// Sends the initial handshake and handles the first client packet.
async fn handshake_inner<S, F>(
    io: &mut PacketIo<MaybeTls<S>>,
    peer: &str,
    connection_id: u64,
    config: &ServerConfig,
    factory: &F,
) -> Result<Outcome<F>, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    let salt = generate_salt();
    let hs = InitialHandshake {
        server_version: &config.server_version,
        connection_id: connection_id as u32,
        salt: &salt,
        capability: config.advertised_capability(),
        collation: collation::UTF8MB4_BIN as u8,
        status: crate::constants::status::AUTOCOMMIT,
        auth_plugin: &config.default_auth_plugin,
    };
    let init_res = async {
        io.write_packet(&hs.encode()).await?;
        io.flush().await
    }
    .await;
    if let Err(e) = init_res {
        // Go pkg/server/conn.go:315-321.
        if e.kind() == io::ErrorKind::UnexpectedEof {
            tidb_base::log_debug!(
                "Could not send handshake due to connection has be closed by client-side",
                "conn" => connection_id,
            );
        } else {
            let err_str = e.to_string();
            tidb_base::log_debug!(
                "Write init handshake to client fail",
                "conn" => connection_id,
                "error" => err_str.as_str(),
            );
        }
        return Err(HandshakeError::from(e));
    }

    let data = io.read_packet().await.map_err(|e| {
        // Go pkg/server/conn.go:563-567.
        if is_packet_eof(&e) {
            tidb_base::log_debug!(
                "wait handshake response fail due to connection has be closed by client-side",
                "conn" => connection_id,
            );
        } else {
            let err_str = e.log_string();
            tidb_base::log_debug!(
                "wait handshake response fail",
                "conn" => connection_id,
                "error" => err_str.as_str(),
            );
        }
        HandshakeError::from(e)
    })?;
    if data.len() >= 4 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) & CLIENT_SSL != 0
    {
        if config.tls.is_some() {
            // SSLRequest: the handshake response follows over TLS (TiDB upgradeToTLS).
            return Ok(Outcome::Upgrade {
                salt,
                seq: io.sequence(),
            });
        }
        // TLS is not advertised, so a conforming client never asks for it.
        // Keep the wire ERR 1235 while matching Go's final `Server.onConn handshake` log error
        // text (`malform packet error`; Go's panic/recover lines are not reproduced).
        let e = SqlError::not_supported("SSL");
        write_handshake_err(io, CLIENT_PROTOCOL_41, connection_id, &e).await;
        return Err(HandshakeError::Other("malform packet error".into()));
    }
    authenticate(io, &data, &salt, peer, connection_id, config, factory)
        .await
        .map(Outcome::Done)
}

/// Parses the handshake response, authenticates and opens the session.
async fn authenticate<S, F>(
    io: &mut PacketIo<MaybeTls<S>>,
    data: &[u8],
    salt: &[u8; crate::auth::SALT_LEN],
    peer: &str,
    connection_id: u64,
    config: &ServerConfig,
    factory: &F,
) -> Result<Accepted<F>, HandshakeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    // Go pkg/server/conn.go:574-583: log malformed response / missing ClientProtocol41 before parse.
    if data.len() < 2 {
        let packet_data = String::from_utf8_lossy(data);
        tidb_base::log_warn!(
            "got malformed handshake response",
            "conn" => connection_id,
            "packetData" => packet_data.as_ref(),
        );
    } else if u32::from(u16::from_le_bytes([data[0], data[1]])) & CLIENT_PROTOCOL_41 == 0 {
        tidb_base::log_warn!(
            "ClientProtocol41 flag is not set, please upgrade client",
            "conn" => connection_id,
        );
    }
    let resp = match HandshakeResponse::parse(data) {
        Ok(r) => r,
        Err(e) => {
            write_handshake_err(io, CLIENT_PROTOCOL_41, connection_id, &e).await;
            return Err(HandshakeError::Other(e.error_for_log()));
        }
    };
    let capability = resp.capability & config.advertised_capability();
    let host = peer_host(peer);
    // Go pkg/server/conn.go:682-691 (`handleAuthPlugin`) -> `checkAuthPlugin` (`conn.go:886-930`):
    // when CLIENT_PLUGIN_AUTH is set, MatchIdentity runs BEFORE any auth switch or
    // openSessionAndDoAuth; an unknown user is rejected with `[server:1045]` and logs
    // `"failed to check the user authplugin"`, returning without logging `:677`.
    let mut auth = resp.auth_response.clone();
    let account = if resp.capability & CLIENT_PLUGIN_AUTH != 0 {
        let Some(account) = factory.lookup_account(&resp.user, &host).await else {
            let e = SqlError::access_denied(&resp.user, &host, !auth.is_empty());
            let err_str = format!("[server:{}]{}", e.code, e.message);
            tidb_base::log_warn!(
                "failed to check the user authplugin",
                "conn" => connection_id,
                "error" => err_str.as_str(),
            );
            write_handshake_err(io, capability, connection_id, &e).await;
            return Err(HandshakeError::Other(err_str));
        };

        // Match Go `checkAuthPlugin` (`pkg/server/conn.go:886-930`): switch to the account's
        // auth plugin whenever the client picked another plugin OR the server advertised another
        // default plugin in InitialHandshake (e.g. libmysqlclient answers mysql_native_password
        // with empty data if InitialHandshake advertised caching_sha2_password).
        let target_plugin = account.credential.plugin_name();
        if resp.auth_plugin.as_deref() != Some(target_plugin)
            || config.default_auth_plugin != target_plugin
        {
            // Go pkg/server/conn.go:287-306 + 686.
            if let Err(e) = io
                .write_packet(&auth_switch_request(target_plugin, salt))
                .await
            {
                let err_str = e.to_string();
                tidb_base::log_debug!(
                    "write response to client failed",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                tidb_base::log_warn!(
                    "failed to check the user authplugin",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                return Err(HandshakeError::from(e));
            }
            if let Err(e) = io.flush().await {
                let err_str = e.to_string();
                tidb_base::log_debug!(
                    "flush response to client failed",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                tidb_base::log_warn!(
                    "failed to check the user authplugin",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                return Err(HandshakeError::from(e));
            }
            auth = io.read_packet().await.map_err(|e| {
                let err_str = e.log_string();
                if is_packet_eof(&e) {
                    tidb_base::log_warn!(
                        "authSwitchRequest response fail due to connection has be closed by client-side",
                        "conn" => connection_id,
                    );
                } else {
                    tidb_base::log_warn!(
                        "authSwitchRequest response fail",
                        "conn" => connection_id,
                        "error" => err_str.as_str(),
                    );
                }
                tidb_base::log_warn!(
                    "failed to check the user authplugin",
                    "conn" => connection_id,
                    "error" => err_str.as_str(),
                );
                HandshakeError::from(e)
            })?;
        }
        Some(account)
    } else {
        factory.lookup_account(&resp.user, &host).await
    };

    let (verified, using_password) = match &account {
        Some(acc) => match &acc.credential {
            cred @ Credential::NativePassword(_) => {
                (cred.verify_native(salt, &auth), !auth.is_empty())
            }
            Credential::CachingSha2Password(None) => {
                // `libmysqlclient` (MySQL 8.0 CLI) sends a single NUL byte `[0]` for an empty
                // `caching_sha2_password` response, while drivers like `mysql_async` send `[]`.
                let empty = auth.is_empty() || auth == [0];
                (empty, !empty)
            }
            cred @ Credential::CachingSha2Password(Some(_)) => {
                let empty = auth.is_empty() || auth == [0];
                // Clients without CLIENT_PLUGIN_AUTH cannot participate in caching_sha2_password
                // AuthMoreData exchanges; an empty initial auth (`[]` or libmysqlclient's `[0]`)
                // against a non-empty hash is rejected immediately with `using password: NO`.
                if resp.capability & CLIENT_PLUGIN_AUTH == 0 || empty {
                    (false, !empty)
                } else {
                    // Go TiDB v8.5.8 (`authSha`, pkg/server/conn.go:722-756) always requests full
                    // authentication (`[0x01, 0x04]`) because it has no fast-auth cache; plaintext
                    // verification is permitted only over TLS (never over unencrypted TCP).
                    if let Err(e) = io
                        .write_packet(&[
                            header::AUTH_MORE_DATA,
                            auth_plugin::CACHING_SHA2_PERFORM_FULL_AUTH,
                        ])
                        .await
                    {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "authSha packet write failed",
                            "conn" => connection_id,
                            "error" => err_str.as_str(),
                        );
                        return Err(HandshakeError::from(e));
                    }
                    if let Err(e) = io.flush().await {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "authSha packet flush failed",
                            "conn" => connection_id,
                            "error" => err_str.as_str(),
                        );
                        return Err(HandshakeError::from(e));
                    }
                    let full_auth = io.read_packet().await.map_err(|e| {
                        let err_str = e.log_string();
                        tidb_base::log_warn!(
                            "authSha packet read failed",
                            "conn" => connection_id,
                            "error" => err_str.as_str(),
                        );
                        HandshakeError::from(e)
                    })?;
                    if io.get_ref().cipher().is_none() {
                        // Never accept cleartext passwords or RSA pubkey requests (0x02) over
                        // plain TCP (Go TiDB v8.5.8 also rejects 0x02 with 1045).
                        (false, true)
                    } else {
                        // Match Go `authSha` (`bytes.Trim(data, "\x00")`, pkg/server/conn.go:755).
                        let start = full_auth
                            .iter()
                            .position(|&b| b != 0)
                            .unwrap_or(full_auth.len());
                        let end = full_auth
                            .iter()
                            .rposition(|&b| b != 0)
                            .map_or(start, |i| i + 1);
                        let plaintext = &full_auth[start..end];
                        (
                            cred.verify_caching_sha2_plaintext(plaintext),
                            !plaintext.is_empty() || !empty,
                        )
                    }
                }
            }
        },
        None => (false, !auth.is_empty()),
    };
    if !verified {
        let e = SqlError::access_denied(&resp.user, &host, using_password);
        let err_str = e.error_for_log();
        tidb_base::log_warn!(
            "open new session or authentication failure",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        write_handshake_err(io, capability, connection_id, &e).await;
        return Err(HandshakeError::Other(err_str));
    }
    let account = account.expect("verified implies Some");
    if account.account_locked {
        let e = SqlError::account_locked(&resp.user, &host);
        let err_str = e.error_for_log();
        tidb_base::log_warn!(
            "open new session or authentication failure",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        write_handshake_err(io, capability, connection_id, &e).await;
        return Err(HandshakeError::Other(err_str));
    }
    if account.password_expired {
        let e = SqlError::password_expired();
        let err_str = e.error_for_log();
        tidb_base::log_warn!(
            "open new session or authentication failure",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        write_handshake_err(io, capability, connection_id, &e).await;
        return Err(HandshakeError::Other(err_str));
    }

    let info = ConnectionInfo {
        connection_id,
        user: resp.user.clone(),
        peer: peer.to_string(),
        auth_user: account.auth_user,
        auth_host: account.auth_host,
        database: resp.database.clone(),
        collation: resp.collation,
        capability,
        tls_cipher: io.get_ref().cipher(),
    };
    let mut handler = match factory.open_session(&info) {
        Ok(h) => h,
        Err(e) => {
            let err_str = e.error_for_log();
            tidb_base::log_warn!(
                "open new session or authentication failure",
                "conn" => connection_id,
                "error" => err_str.as_str(),
            );
            write_handshake_err(io, capability, connection_id, &e).await;
            return Err(HandshakeError::Other(err_str));
        }
    };
    if let Some(db) = &resp.database
        && let Err(e) = handler.use_db(db).await
    {
        let err_str = e.error_for_log();
        tidb_base::log_warn!(
            "open new session or authentication failure",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        write_handshake_err(io, capability, connection_id, &e).await;
        return Err(HandshakeError::Other(err_str));
    }

    let mut ok = vec![header::OK, 0, 0];
    ok.extend_from_slice(&handler.status_flags().to_le_bytes());
    ok.extend_from_slice(&[0, 0]);
    // Go pkg/server/conn.go:350-363.
    if let Err(e) = io.write_packet(&ok).await {
        let err_str = e.to_string();
        tidb_base::log_debug!(
            "write response to client failed",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        return Err(HandshakeError::from(e));
    }
    if let Err(e) = io.flush().await {
        let err_str = e.to_string();
        tidb_base::log_debug!(
            "flush response to client failed",
            "conn" => connection_id,
            "error" => err_str.as_str(),
        );
        return Err(HandshakeError::from(e));
    }
    Ok((handler, capability, resp.user))
}

/// Go `pkg/types/datum.go:2445` `isPrintable`: valid UTF-8 with no Unicode control characters.
fn is_printable_datum_bytes(bytes: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(bytes).ok()?;
    if s.chars().any(char::is_control) {
        return None;
    }
    Some(s)
}

/// Format bound `COM_STMT_EXECUTE` / `COM_STMT_FETCH` parameter matching Go
/// `pkg/types/datum.go:2467-2525` (`datumsToString` with `handleSpecialValue=true, binaryAsHex=true`).
fn format_param_value(p: &crate::params::Param) -> String {
    use std::fmt::Write as _;

    use crate::value::Value;
    const LOG_DATUM_LEN: usize = 2048;

    match &p.value {
        Value::Null => "NULL".to_owned(),
        Value::Bytes(b) => {
            let original_len = (b.len() > LOG_DATUM_LEN).then_some(b.len());
            let slice = if b.len() > LOG_DATUM_LEN {
                &b[..LOG_DATUM_LEN]
            } else {
                b.as_slice()
            };
            let mut out = if let Some(s) = is_printable_datum_bytes(slice) {
                let mut q = String::with_capacity(s.len() + 2);
                q.push('"');
                q.push_str(s);
                q.push('"');
                q
            } else {
                let mut h = String::with_capacity(2 + slice.len() * 2);
                h.push_str("0x");
                for byte in slice {
                    let _ = write!(&mut h, "{byte:02X}");
                }
                h
            };
            if let Some(orig) = original_len {
                let _ = write!(&mut out, " len({orig})");
            }
            out
        }
        other => {
            let s = match other {
                Value::Int(i) => i.to_string(),
                Value::UInt(u) => u.to_string(),
                Value::Float(f) => f.to_string(),
                Value::Double(f) => f.to_string(),
                Value::Decimal(s) => s.clone(),
                Value::DateTime(dt) => {
                    if dt.microsecond > 0 {
                        format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
                            dt.year,
                            dt.month,
                            dt.day,
                            dt.hour,
                            dt.minute,
                            dt.second,
                            dt.microsecond
                        )
                    } else {
                        format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                            dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
                        )
                    }
                }
                Value::Time(d) => {
                    let sign = if d.negative { "-" } else { "" };
                    if d.microseconds > 0 {
                        format!(
                            "{sign}{:02}:{:02}:{:02}.{:06}",
                            d.hours, d.minutes, d.seconds, d.microseconds
                        )
                    } else {
                        format!("{sign}{:02}:{:02}:{:02}", d.hours, d.minutes, d.seconds)
                    }
                }
                Value::Null | Value::Bytes(_) => unreachable!(),
            };
            if s.len() > LOG_DATUM_LEN {
                let orig = s.len();
                let mut trunc =
                    String::from_utf8_lossy(&s.as_bytes()[..LOG_DATUM_LEN]).into_owned();
                let _ = write!(&mut trunc, " len({orig})");
                trunc
            } else {
                s
            }
        }
    }
}

/// Format a `COM_STMT_EXECUTE` / `COM_STMT_FETCH` statement + parameters for
/// `command dispatched failed` (Go `conn_stmt.go:660-670` `preparedStmt2String` + `executor.FormatSQL`).
pub fn format_stmt_execute_sql_for_log(
    sql: &str,
    params: &[crate::params::Param],
    redact_mode: &str,
) -> String {
    let mut out = tidb_parser::normalize(sql, redact_mode);
    if redact_mode != tidb_base::errors::RedactLogEnable && !params.is_empty() {
        let formatted = if params.len() == 1 {
            format!(" [arguments: {}]", format_param_value(&params[0]))
        } else {
            let joined: Vec<String> = params.iter().map(format_param_value).collect();
            format!(" [arguments: ({})]", joined.join(", "))
        };
        out.push_str(&tidb_base::redact::string(redact_mode, &formatted));
    }
    format_sql_for_conn_log(&out, current_query_log_max_len())
}

/// Format the last command body for `command dispatched failed` (Go `conn.go:2742-2768`),
/// normalizing SQL via `tidb_parser::normalize` when `redact_mode != "OFF"`.
fn format_cmd_sql_for_log(
    cmd: u8,
    body: &[u8],
    stmts: &HashMap<u32, StmtState>,
    redact_mode: &str,
) -> String {
    let max_len = current_query_log_max_len();
    match cmd {
        command::QUERY | command::STMT_PREPARE => {
            let raw = String::from_utf8_lossy(body);
            // Strip at most one trailing NUL (Go `string(data[:len-1])` when NUL-terminated).
            let raw = raw.strip_suffix('\0').unwrap_or(&raw);
            let normalized = tidb_parser::normalize(raw, redact_mode);
            format_sql_for_conn_log(&normalized, max_len)
        }
        command::STMT_EXECUTE | command::STMT_FETCH if body.len() >= 4 => {
            let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if let Some(st) = stmts.get(&id) {
                format_stmt_execute_sql_for_log(&st.sql, &st.last_params, redact_mode)
            } else {
                tidb_base::mysql::Command2Str
                    .get(&cmd)
                    .copied()
                    .unwrap_or("Execute")
                    .to_owned()
            }
        }
        command::INIT_DB => format!("Use {}", String::from_utf8_lossy(body)),
        command::FIELD_LIST => format!("ListFields {}", String::from_utf8_lossy(body)),
        command::STMT_CLOSE | command::STMT_RESET if body.len() >= 4 => {
            let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            let name = tidb_base::mysql::Command2Str
                .get(&cmd)
                .copied()
                .unwrap_or("Stmt");
            format!("{name} {id}")
        }
        _ => tidb_base::mysql::Command2Str
            .get(&cmd)
            .copied()
            .map(str::to_owned)
            .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned()),
    }
}

/// Host part of an `ip:port` peer address, normalizing IPv4-mapped IPv6 (`::ffff:a.b.c.d`)
/// to dotted IPv4 so dual-stack `::` listeners match IPv4 host grants like `'10.%'`.
fn peer_host(peer: &str) -> std::borrow::Cow<'_, str> {
    let raw = match peer.rsplit_once(':') {
        Some((host, _)) => host.trim_start_matches('[').trim_end_matches(']'),
        None => peer,
    };
    if let Ok(std::net::IpAddr::V6(v6)) = raw.parse::<std::net::IpAddr>()
        && let Some(v4) = v6.to_ipv4_mapped()
    {
        return std::borrow::Cow::Owned(v4.to_string());
    }
    std::borrow::Cow::Borrowed(raw)
}

#[cfg(test)]
mod log_format_tests {
    use super::*;

    #[test]
    fn format_sql_folds_whitespace_and_truncates() {
        assert_eq!(
            format_sql_for_conn_log("SELECT\n  1,\r\n\tFROM t", 4096),
            "SELECT   1,   FROM t"
        );
        assert_eq!(
            format_sql_for_conn_log("SELECT 1234567890", 10),
            "SELECT 123(len:17)"
        );
    }

    #[test]
    fn err_str_for_log_matches_go_matrix() {
        // 1. Table not exists (1146): no stack even when source_err carries stack.
        let tbl_src = tidb_base::terror::ClassSchema
            .new_std(tidb_base::errno::ErrNoSuchTable)
            .gen_with_stack_by_args(tidb_base::go_args!["test", "no_such_table"]);
        assert!(tbl_src.has_stack());
        let tbl_err = SqlError::new(1146, "Table 'test.no_such_table' doesn't exist")
            .with_source_err(tbl_src.clone());
        assert_eq!(
            err_str_for_log(&tbl_err, tidb_base::errors::RedactLogDisable),
            tbl_src.error()
        );

        // 2. Parse error (1064): redacted fixed message when redact != OFF; no stack when OFF.
        let parse_src = tidb_base::terror::ClassParser
            .new_std(tidb_base::errno::ErrParse)
            .gen_with_stack_by_args(tidb_base::go_args!["syntax", 1]);
        let parse_err = SqlError::new(1064, "syntax error").with_source_err(parse_src.clone());
        assert_eq!(
            err_str_for_log(&parse_err, tidb_base::errors::RedactLogDisable),
            parse_src.error()
        );
        assert_eq!(
            err_str_for_log(&parse_err, tidb_base::errors::RedactLogEnable),
            b"fail to parse SQL, and must redact the whole error when enable log redaction"
        );

        // 3. Overflow error (1690): includes stack trace from error_stack().
        let ovf_src = tidb_base::terror::ClassTypes
            .new_std(tidb_base::errno::ErrDataOutOfRange)
            .gen_with_stack_by_args(tidb_base::go_args!["BIGINT", "(9223372036854775807 + 1)"]);
        let ovf_err =
            SqlError::new(1690, "BIGINT value is out of range").with_source_err(ovf_src.clone());
        let got = err_str_for_log(&ovf_err, tidb_base::errors::RedactLogDisable);
        assert_eq!(got, ovf_src.error_stack());
        assert!(
            got.len() > ovf_src.error().len(),
            "must include stack trace"
        );
    }
}

struct Conn<S, H> {
    io: PacketIo<S>,
    capability: u32,
    handler: H,
    peer: String,
    user: String,
    last_sql: String,
    stmts: HashMap<u32, StmtState>,
    next_stmt_id: u32,
    /// When the server started (for COM_STATISTICS uptime).
    started: Instant,
    /// When the previous command finished (Go `clientConn.lastActive`), for
    /// `tidb_server_conn_idle_duration_seconds`.
    last_active: Instant,
    /// Q1 statement admission (Go `server.concurrentLimiter`, `token-limit`); `None` admits
    /// everything (tests).
    admission: Option<tidb_runtime::Admission>,
    /// The token of the command in progress (Go: taken in `dispatch`, put back in its defer).
    token: Option<crate::admission::HeldToken>,
    /// Shared status for graceful drain (reading vs dispatching, in-txn vs autocommit).
    status: Arc<ConnStatus>,
}

impl<S: AsyncRead + AsyncWrite + Unpin, H: SessionHandler> Conn<S, H> {
    async fn command_loop(&mut self, connection_id: u64) {
        loop {
            self.io.reset_sequence();
            let in_txn = (self.handler.status_flags() & crate::constants::status::IN_TRANS) != 0;
            self.status.set_reading(true, in_txn);
            let data = match self.io.read_packet().await {
                Ok(d) => {
                    self.status.set_reading(false, in_txn);
                    d
                }
                Err(PacketError::Eof) => {
                    // Go: any read failure, a client closing without COM_QUIT included.
                    tidb_metrics::disconnected(tidb_metrics::Disconnect::Error);
                    return;
                }
                Err(PacketError::TooLarge) => {
                    let e = SqlError::new(
                        code::ER_NET_PACKET_TOO_LARGE,
                        PacketError::TooLarge.to_string(),
                    );
                    let _ = write_err(&mut self.io, self.capability, &e).await;
                    tidb_metrics::disconnected(tidb_metrics::Disconnect::Error);
                    return;
                }
                Err(e) => {
                    let err_str = e.log_string();
                    if !err_str.contains("use of closed network connection") {
                        tidb_base::log_warn!(
                            "read packet failed, close this connection",
                            "conn" => connection_id,
                            "session_alias" => self.handler.session_alias(),
                            "error" => &err_str,
                        );
                    }
                    tidb_metrics::disconnected(tidb_metrics::Disconnect::Error);
                    return;
                }
            };
            // Go `clientConn.Run` / `dispatch` / `addMetrics`.
            let start = Instant::now();
            tidb_metrics::command_started(start.saturating_duration_since(self.last_active));
            // Go `dispatch`: `token := cc.server.getToken()` before running the command; it
            // waits, never rejects. `releaseToken` is in `command_metrics` (Go's defer).
            self.token = Some(crate::admission::HeldToken::acquire(self.admission.as_ref()).await);
            let cmd = data.first().copied();
            let result = match data.split_first() {
                None => Err(ConnError::Sql(SqlError::malformed())),
                Some((&command::QUIT, _)) => {
                    self.command_metrics(command::QUIT, start, None);
                    tidb_metrics::disconnected(tidb_metrics::Disconnect::Ok);
                    return;
                }
                Some((&cmd, body)) => {
                    self.last_sql =
                        format_cmd_sql_for_log(cmd, body, &self.stmts, self.handler.redact_mode());
                    // Go `clientConn.Run`'s deferred recover: a panic while running a command
                    // is logged, written to the client as an error, counted
                    // (`tidb_server_panic_total{type="session"}`), and the connection is
                    // closed. The session is dropped with the connection, so no state of the
                    // half-run command survives.
                    match tidb_base::panic_guard::catch_unwind_future(self.dispatch(cmd, body))
                        .await
                    {
                        Ok(r) => r,
                        Err((payload, bt)) => {
                            let msg =
                                tidb_base::panic_guard::panic_payload_message(payload.as_ref());
                            let stack = bt.unwrap_or_default();
                            tidb_base::log_error!(
                                "connection running loop panic",
                                "conn" => connection_id,
                                "session_alias" => self.handler.session_alias(),
                                "lastSQL" => &self.last_sql,
                                "err" => &msg,
                                "stack" => &stack,
                            );
                            let e = SqlError::unknown(msg);
                            let _ = write_err(&mut self.io, self.capability, &e).await;
                            tidb_metrics::panicked(tidb_metrics::PANIC_SESSION);
                            self.command_metrics(cmd, start, Some("unknown"));
                            tidb_metrics::disconnected(tidb_metrics::Disconnect::Error);
                            return;
                        }
                    }
                }
            };
            let (error, keep_going) = match result {
                Ok(()) => (None, true),
                Err(ConnError::Sql(e)) => {
                    // Go conn.go:1194-1201: ErrResultUndetermined closes connection; ErrCritical
                    // fatals the server.
                    let mut fatal_close = false;
                    if let Some(src) = &e.source_err {
                        if tidb_base::terror::ErrCritical.equal(src) {
                            tidb_base::log_fatal!("critical error, stop the server", "error" => src);
                            let _ = tidb_base::structured_log::logger().sync();
                            std::process::exit(1);
                        }
                        if tidb_base::terror::ErrResultUndetermined.equal(src) {
                            tidb_base::log_warn!(
                                "result undetermined, close this connection",
                                "error" => src,
                            );
                            fatal_close = true;
                        }
                    }
                    // Go conn.go:1212-1230: ErrLockAcquireFailAndNoWaitSet (3572) logs DEBUG
                    // and skips `command dispatched failed`; all other errors log the exact
                    // 9-field INFO `command dispatched failed`.
                    if !fatal_close && e.code == 3572 {
                        if let Some(src) = &e.source_err {
                            tidb_base::log_debug!(
                                "Expected error for FOR UPDATE NOWAIT",
                                "error" => src,
                            );
                        } else {
                            tidb_base::log_debug!(
                                "Expected error for FOR UPDATE NOWAIT",
                                "error" => &e.message,
                            );
                        }
                    } else if !fatal_close {
                        let status_flags = self.handler.status_flags();
                        let in_txn =
                            u8::from((status_flags & crate::constants::status::IN_TRANS) != 0);
                        let autocommit =
                            u8::from((status_flags & crate::constants::status::AUTOCOMMIT) != 0);
                        let conn_info = format!(
                            "id:{connection_id}, addr:{} status:{status_flags:b}, collation:{}, user:{}",
                            self.peer,
                            self.handler.collation_name(),
                            self.user,
                        );
                        let cmd_byte = cmd.unwrap_or(command::SLEEP);
                        let cmd_name = tidb_base::mysql::Command2Str
                            .get(&cmd_byte)
                            .copied()
                            .unwrap_or("Unknown");
                        let status_str = format!("inTxn:{in_txn}, autocommit:{autocommit}");
                        let err_bytes = err_str_for_log(&e, self.handler.redact_mode());
                        tidb_base::log_info!(
                            "command dispatched failed",
                            "conn" => connection_id,
                            "session_alias" => self.handler.session_alias(),
                            "connInfo" => &conn_info,
                            "command" => cmd_name,
                            "status" => &status_str,
                            "sql" => &self.last_sql,
                            "txn_mode" => self.handler.txn_mode(),
                            "timestamp" => self.handler.txn_timestamp(),
                            "err" => err_bytes.as_slice(),
                        );
                    }
                    let label = tidb_metrics::error_label(e.code);
                    if fatal_close {
                        // Go conn.go:1194-1197: on ErrResultUndetermined, close the connection
                        // immediately without sending an error packet to the client.
                        (Some(label), false)
                    } else {
                        match write_err(&mut self.io, self.capability, &e).await {
                            Ok(()) => (Some(label), true),
                            Err(e) => {
                                let err_str = e.to_string();
                                tidb_base::log_debug!(
                                    "write error packet failed",
                                    "conn" => connection_id,
                                    "error" => err_str.as_str(),
                                );
                                (Some(label), false)
                            }
                        }
                    }
                }
                Err(ConnError::Fatal(e)) => {
                    let err_str = e.to_string();
                    tidb_base::log_debug!(
                        "connection error",
                        "conn" => connection_id,
                        "error" => err_str.as_str(),
                    );
                    (Some("unknown"), false)
                }
            };
            self.command_metrics(cmd.unwrap_or(command::SLEEP), start, error);
            if !keep_going {
                tidb_metrics::disconnected(tidb_metrics::Disconnect::Error);
                return;
            }
        }
    }

    /// Record a finished command: `query_total`, `handle_query_duration_seconds` (by the
    /// session's statement type, which, as Go's `StmtCtx.StmtType`, is still the previous
    /// statement's for commands that run none), `execute_error_total`, the packet bytes; then
    /// the connection is idle again.
    fn command_metrics(&mut self, cmd: u8, start: Instant, error: Option<&str>) {
        let now = Instant::now();
        tidb_metrics::command_finished(cmd, self.handler.stmt_type(), now - start, error);
        // Go's deferred releaseToken: the token and the gauge (HeldToken's Drop).
        self.token = None;
        self.io.flush_counts();
        self.last_active = now;
    }

    async fn dispatch(&mut self, cmd: u8, body: &[u8]) -> Result<(), ConnError> {
        match cmd {
            command::INIT_DB => {
                let db = std::str::from_utf8(body).map_err(|_| SqlError::malformed())?;
                self.handler.use_db(db).await?;
                self.write_ok(&OkInfo::default()).await
            }
            command::QUERY => {
                let sql = sql_text(body)?;
                let start = Instant::now();
                let resp = self.handler.query(sql).await;
                self.write_timed(start, resp, false).await
            }
            command::PING => self.write_ok(&OkInfo::default()).await,
            command::STATISTICS => {
                let s = format!(
                    "Uptime: {}  Threads: 0  Questions: 0  Open tables: 0",
                    self.started.elapsed().as_secs()
                );
                self.io.write_packet(s.as_bytes()).await?;
                Ok(self.io.flush().await?)
            }
            command::STMT_PREPARE => self.stmt_prepare(body).await,
            command::STMT_EXECUTE => self.stmt_execute(body).await,
            command::STMT_SEND_LONG_DATA => self.stmt_send_long_data(body),
            command::STMT_CLOSE => {
                if body.len() >= 4 {
                    let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                    if self.stmts.remove(&id).is_some() {
                        self.handler.close(id);
                    }
                }
                Ok(())
            }
            command::STMT_RESET => {
                let id = Reader::new(body).u32()?;
                let stmt = self
                    .stmts
                    .get_mut(&id)
                    .ok_or_else(|| SqlError::unknown_stmt(id, "stmt_reset"))?;
                stmt.long_data.iter_mut().for_each(|d| *d = None);
                stmt.last_params.clear();
                self.write_ok(&OkInfo::default()).await
            }
            command::SET_OPTION => match Reader::new(body).u16()? {
                // MYSQL_OPTION_MULTI_STATEMENTS_ON
                0 => Err(SqlError::not_supported("multi-statements").into()),
                // MYSQL_OPTION_MULTI_STATEMENTS_OFF: already off.
                1 => {
                    self.write_eof(0).await?;
                    Ok(self.io.flush().await?)
                }
                _ => Err(SqlError::malformed().into()),
            },
            command::RESET_CONNECTION => {
                for id in std::mem::take(&mut self.stmts).into_keys() {
                    self.handler.close(id);
                }
                self.write_ok(&OkInfo::default()).await
            }
            command::FIELD_LIST => Err(SqlError::not_supported("COM_FIELD_LIST").into()),
            command::STMT_FETCH => Err(SqlError::not_supported("COM_STMT_FETCH").into()),
            command::CHANGE_USER => Err(SqlError::not_supported("COM_CHANGE_USER").into()),
            _ => Err(SqlError::unknown(format!("command {cmd} not supported now")).into()),
        }
    }

    async fn stmt_prepare(&mut self, body: &[u8]) -> Result<(), ConnError> {
        let sql = sql_text(body)?;
        let id = self.next_stmt_id;
        self.next_stmt_id = self.next_stmt_id.checked_add(1).unwrap_or(1);
        let info = self.handler.prepare(id, sql).await?;
        let param_count = info.param_count;
        self.stmts.insert(
            id,
            StmtState {
                sql: sql.to_owned(),
                param_count,
                param_types: Vec::new(),
                long_data: vec![None; usize::from(param_count)],
                last_params: Vec::new(),
            },
        );

        let mut b = Vec::with_capacity(12);
        b.push(header::OK);
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&(info.columns.len() as u16).to_le_bytes());
        b.extend_from_slice(&param_count.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&0u16.to_le_bytes());
        self.io.write_packet(&b).await?;

        if param_count > 0 {
            // TiDB describes every parameter as an anonymous BLOB.
            let param = ColumnDef {
                charset: 0,
                column_length: 0,
                flags: 0,
                org_name: String::new(),
                ..ColumnDef::new("", ColumnType::Blob)
            };
            for _ in 0..param_count {
                b.clear();
                param.encode(&mut b);
                self.io.write_packet(&b).await?;
            }
            if self.capability & CLIENT_DEPRECATE_EOF == 0 {
                self.write_eof(0).await?;
            }
        }
        if !info.column_packets.is_empty() {
            for p in &info.column_packets {
                self.io.write_packet(p).await?;
            }
            if self.capability & CLIENT_DEPRECATE_EOF == 0 {
                self.write_eof(0).await?;
            }
        } else if !info.columns.is_empty() {
            for c in &info.columns {
                b.clear();
                c.encode(&mut b);
                self.io.write_packet(&b).await?;
            }
            if self.capability & CLIENT_DEPRECATE_EOF == 0 {
                self.write_eof(0).await?;
            }
        }
        Ok(self.io.flush().await?)
    }

    async fn stmt_execute(&mut self, body: &[u8]) -> Result<(), ConnError> {
        let start = Instant::now();
        if body.len() < 9 {
            return Err(SqlError::malformed().into());
        }
        let mut r = Reader::new(body);
        let id = r.u32()?;
        let stmt = self
            .stmts
            .get_mut(&id)
            .ok_or_else(|| SqlError::unknown_stmt(id, "stmt_execute"))?;
        let flags = r.u8()?;
        if flags & cursor_type::FOR_UPDATE != 0 {
            return Err(SqlError::unknown("unsupported flag: CursorTypeForUpdate").into());
        }
        if flags & cursor_type::SCROLLABLE != 0 {
            return Err(SqlError::unknown("unsupported flag: CursorTypeScrollable").into());
        }
        // CURSOR_TYPE_READ_ONLY is accepted but no cursor is opened: the full result set is
        // returned without SERVER_STATUS_CURSOR_EXISTS, which clients handle as a plain result.
        r.skip(4)?; // iteration count, always 1

        let n = usize::from(stmt.param_count);
        let mut raw = RawParams::default();
        let params = if n > 0 {
            let null_bitmap = r.take(n.div_ceil(8))?;
            if r.u8()? == 1 {
                stmt.param_types = r.take(2 * n)?.to_vec();
            }
            // `rest` consumes the reader: the PoC's decoder and the raw section (the new
            // stack's parseBinaryParams) read the same bytes.
            let values = r.rest();
            let result = decode_params(n, &stmt.long_data, null_bitmap, &stmt.param_types, values);
            raw = RawParams {
                null_bitmap: null_bitmap.to_vec(),
                param_types: stmt.param_types.clone(),
                param_values: values.to_vec(),
                // Long data is consumed by an execute, successful or not (TiDB resets the
                // stmt).
                bound: stmt.long_data.iter_mut().map(Option::take).collect(),
            };
            result?
        } else {
            Vec::new()
        };
        let stmt_sql = stmt.sql.clone();
        stmt.last_params = params.clone();
        self.last_sql =
            format_stmt_execute_sql_for_log(&stmt_sql, &params, self.handler.redact_mode());
        self.handler.execute_flags(id, flags);
        self.handler.execute_raw_params(id, raw);
        let resp = self.handler.execute(id, params).await;
        self.write_timed(start, resp, true).await
    }

    /// Write a statement's response and report the timings to the handler.
    async fn write_timed(
        &mut self,
        start: Instant,
        resp: Result<Response, SqlError>,
        binary: bool,
    ) -> Result<(), ConnError> {
        let written = Instant::now();
        let result = match resp {
            Ok(resp) => self.write_response(resp, binary).await,
            Err(e) => Err(e.into()),
        };
        let end = Instant::now();
        self.handler.command_done(end - written, end - start);
        result
    }

    fn stmt_send_long_data(&mut self, body: &[u8]) -> Result<(), ConnError> {
        if body.len() < 6 {
            return Err(SqlError::malformed().into());
        }
        let mut r = Reader::new(body);
        let id = r.u32()?;
        let param = usize::from(r.u16()?);
        let stmt = self
            .stmts
            .get_mut(&id)
            .ok_or_else(|| SqlError::unknown_stmt(id, "stmt_send_longdata"))?;
        let slot = stmt.long_data.get_mut(param).ok_or_else(|| {
            SqlError::new(
                code::ER_WRONG_ARGUMENTS,
                "Incorrect arguments to stmt_send_longdata",
            )
        })?;
        slot.get_or_insert_with(Vec::new)
            .extend_from_slice(r.rest());
        // No response on success.
        Ok(())
    }

    async fn write_response(&mut self, resp: Response, binary: bool) -> Result<(), ConnError> {
        match resp {
            Response::Ok(info) => self.write_ok(&info).await,
            Response::ResultSet(rs) if rs.columns.is_empty() => {
                self.write_ok(&OkInfo {
                    warnings: rs.warnings,
                    ..Default::default()
                })
                .await
            }
            Response::ResultSet(rs) => self.write_result_set(&rs, binary).await,
            Response::Encoded(rs) if rs.column_packets.is_empty() => {
                self.write_ok(&OkInfo {
                    warnings: rs.warnings,
                    ..Default::default()
                })
                .await
            }
            Response::Encoded(rs) => self.write_encoded_result_set(&rs).await,
        }
    }

    /// A result set encoded by the caller (Go's `writeChunks`: column count, column
    /// definitions, EOF for old clients, rows, then EOF or the OK packet with the EOF
    /// header).
    async fn write_encoded_result_set(&mut self, rs: &EncodedResultSet) -> Result<(), ConnError> {
        let status = self.handler.status_flags();
        let mut b = Vec::with_capacity(16);
        encode_column_count(&mut b, rs.column_packets.len());
        self.io.write_packet(&b).await?;
        for p in &rs.column_packets {
            self.io.write_packet(p).await?;
        }
        if self.capability & CLIENT_DEPRECATE_EOF == 0 {
            self.write_eof(rs.warnings).await?;
        }
        for row in &rs.rows {
            self.io.write_packet(row).await?;
        }
        if self.capability & CLIENT_DEPRECATE_EOF != 0 {
            self.write_ok_with_header(
                header::EOF,
                &OkInfo {
                    warnings: rs.warnings,
                    ..Default::default()
                },
                status,
            )
            .await?;
        } else {
            self.write_eof(rs.warnings).await?;
        }
        Ok(self.io.flush().await?)
    }

    async fn write_result_set(&mut self, rs: &ResultSet, binary: bool) -> Result<(), ConnError> {
        let status = self.handler.status_flags();
        let mut b = Vec::with_capacity(256);
        encode_column_count(&mut b, rs.columns.len());
        self.io.write_packet(&b).await?;
        for c in &rs.columns {
            b.clear();
            c.encode(&mut b);
            self.io.write_packet(&b).await?;
        }
        if self.capability & CLIENT_DEPRECATE_EOF == 0 {
            self.write_eof(rs.warnings).await?;
        }
        for row in &rs.rows {
            b.clear();
            if binary {
                encode_binary_row(&mut b, &rs.columns, row)?;
            } else {
                encode_text_row(&mut b, &rs.columns, row)?;
            }
            self.io.write_packet(&b).await?;
        }
        if self.capability & CLIENT_DEPRECATE_EOF != 0 {
            // OK packet with the EOF header terminates the rows.
            self.write_ok_with_header(
                header::EOF,
                &OkInfo {
                    warnings: rs.warnings,
                    ..Default::default()
                },
                status,
            )
            .await?;
        } else {
            self.write_eof(rs.warnings).await?;
        }
        Ok(self.io.flush().await?)
    }

    async fn write_ok(&mut self, info: &OkInfo) -> Result<(), ConnError> {
        let status = self.handler.status_flags();
        self.write_ok_with_header(header::OK, info, status).await?;
        Ok(self.io.flush().await?)
    }

    /// OK packet (TiDB `writeOkWith`); not flushed.
    async fn write_ok_with_header(
        &mut self,
        hdr: u8,
        info: &OkInfo,
        status: u16,
    ) -> io::Result<()> {
        let mut b = Vec::with_capacity(16 + info.info.len());
        b.push(hdr);
        put_lenenc_int(&mut b, info.affected_rows);
        put_lenenc_int(&mut b, info.last_insert_id);
        if self.capability & CLIENT_PROTOCOL_41 != 0 {
            b.extend_from_slice(&status.to_le_bytes());
            b.extend_from_slice(&info.warnings.to_le_bytes());
        }
        if !info.info.is_empty() {
            put_lenenc_str(&mut b, info.info.as_bytes());
        }
        self.io.write_packet(&b).await
    }

    /// EOF packet, or an OK packet with the EOF header for `CLIENT_DEPRECATE_EOF` clients
    /// (TiDB `writeEOF`); not flushed.
    async fn write_eof(&mut self, warnings: u16) -> io::Result<()> {
        let status = self.handler.status_flags();
        if self.capability & CLIENT_DEPRECATE_EOF != 0 {
            return self
                .write_ok_with_header(
                    header::EOF,
                    &OkInfo {
                        warnings,
                        ..Default::default()
                    },
                    status,
                )
                .await;
        }
        let mut b = vec![header::EOF];
        if self.capability & CLIENT_PROTOCOL_41 != 0 {
            b.extend_from_slice(&warnings.to_le_bytes());
            b.extend_from_slice(&status.to_le_bytes());
        }
        self.io.write_packet(&b).await
    }
}

/// SQL text of `COM_QUERY`/`COM_STMT_PREPARE`, with one trailing NUL removed (TiDB issue 1989).
fn sql_text(body: &[u8]) -> Result<&str, SqlError> {
    let body = body.strip_suffix(&[0]).unwrap_or(body);
    std::str::from_utf8(body).map_err(|_| SqlError::unknown("invalid UTF-8 in SQL text"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_host_strips_port() {
        assert_eq!(peer_host("127.0.0.1:5000"), "127.0.0.1");
        assert_eq!(peer_host("[::1]:5000"), "::1");
        assert_eq!(peer_host("[::ffff:10.0.0.5]:5000"), "10.0.0.5");
        assert_eq!(peer_host("[::FFFF:127.0.0.1]:5000"), "127.0.0.1");
        assert_eq!(peer_host("local"), "local");
    }

    #[test]
    fn sql_text_trims_one_nul() {
        assert_eq!(sql_text(b"select 1\0").unwrap(), "select 1");
        assert_eq!(sql_text(b"select 1").unwrap(), "select 1");
        assert!(sql_text(&[0xff]).is_err());
    }
}
