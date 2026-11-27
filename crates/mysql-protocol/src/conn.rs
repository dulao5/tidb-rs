//! One client connection: handshake, authentication and the command loop
//! (TiDB `server/conn.go`, `server/conn_stmt.go`).

use std::collections::HashMap;
use std::io;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, trace, warn};

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
}

/// Runs a connection to completion. Errors are logged, never propagated.
pub(crate) async fn run<S, F>(
    stream: S,
    peer: String,
    connection_id: u64,
    started: Instant,
    config: &ServerConfig,
    factory: &F,
    admission: Option<tidb_runtime::Admission>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    // Go counts the connection from accept (`newConn`) until it is closed.
    let _gauge = ConnectionGauge::new();
    let accepted = Instant::now();
    let io = PacketIo::new(MaybeTls::Plain(stream), config.max_allowed_packet);
    let mut conn = match handshake(io, &peer, connection_id, config, factory).await {
        Ok((mut io, Some((handler, capability, user)))) => {
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
            }
        }
        Ok((_, None)) => return,
        Err(e) => {
            debug!(connection_id, %peer, error = %e, "handshake failed");
            return;
        }
    };
    debug!(connection_id, %peer, "connection established");
    conn.command_loop(connection_id).await;
    for id in std::mem::take(&mut conn.stmts).into_keys() {
        conn.handler.close(id);
    }
    let _ = conn.io.shutdown().await;
    debug!(connection_id, "connection closed");
}

/// Default `QueryLogMaxLen` matching Go `executor.FormatSQL` (4096 bytes).
pub const QUERY_LOG_MAX_LEN: usize = 4096;

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

/// The session, effective capability flags, and authenticated username.
type Accepted<F> = Option<(<F as SessionFactory>::Session, u32, String)>;

/// Performs the connection phase, including the TLS upgrade after an SSLRequest. Returns the
/// (possibly upgraded) packet stream, and `None` after an orderly rejection (an ERR packet was
/// sent); `Err` on I/O or TLS failure.
async fn handshake<S, F>(
    mut io: PacketIo<MaybeTls<S>>,
    peer: &str,
    connection_id: u64,
    config: &ServerConfig,
    factory: &F,
) -> Result<(PacketIo<MaybeTls<S>>, Accepted<F>), String>
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
                return Err("TLS upgrade without TLS config".into());
            };
            let (stream, buffered) = io.into_parts();
            let MaybeTls::Plain(stream) = stream else {
                return Err("TLS requested twice".into());
            };
            let acceptor = tokio_rustls::TlsAcceptor::from(tls.clone());
            let tls_stream = acceptor
                .accept(Rewind::new(buffered, stream))
                .await
                .map_err(|e| format!("TLS handshake failed: {e}"))?;
            let mut io = PacketIo::new(
                MaybeTls::Tls(Box::new(tls_stream)),
                config.max_allowed_packet,
            );
            io.set_sequence(seq);
            let data = io.read_packet().await.map_err(|e| e.to_string())?;
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
) -> Result<Outcome<F>, String>
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
    let io_err = |e: io::Error| e.to_string();
    io.write_packet(&hs.encode()).await.map_err(io_err)?;
    io.flush().await.map_err(io_err)?;

    let data = io.read_packet().await.map_err(|e| e.to_string())?;
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
        let e = SqlError::not_supported("SSL");
        write_err(io, CLIENT_PROTOCOL_41, &e)
            .await
            .map_err(io_err)?;
        return Ok(Outcome::Done(None));
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
) -> Result<Accepted<F>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    let io_err = |e: io::Error| e.to_string();
    let resp = match HandshakeResponse::parse(data) {
        Ok(r) => r,
        Err(e) => {
            write_err(io, CLIENT_PROTOCOL_41, &e)
                .await
                .map_err(io_err)?;
            return Ok(None);
        }
    };
    let capability = resp.capability & config.advertised_capability();

    // Only mysql_native_password is verified; any other plugin the client picked (typically
    // caching_sha2_password, libmysqlclient 8.x's default) is switched to it, which is what
    // TiDB does for accounts using mysql_native_password. Like TiDB (`checkAuthPlugin`), also
    // switch when the handshake advertised another plugin: libmysqlclient then answers
    // mysql_native_password with *empty* data, since the salt was offered to another plugin.
    let mut auth = resp.auth_response.clone();
    if resp.capability & CLIENT_PLUGIN_AUTH != 0
        && (resp.auth_plugin.as_deref() != Some(auth_plugin::MYSQL_NATIVE_PASSWORD)
            || config.default_auth_plugin != auth_plugin::MYSQL_NATIVE_PASSWORD)
    {
        io.write_packet(&auth_switch_request(
            auth_plugin::MYSQL_NATIVE_PASSWORD,
            salt,
        ))
        .await
        .map_err(io_err)?;
        io.flush().await.map_err(io_err)?;
        auth = io.read_packet().await.map_err(|e| e.to_string())?;
    }

    let host = peer_host(peer);
    let verified = match factory.lookup_user(&resp.user, host).await {
        Some(cred @ Credential::NativePassword(_)) => cred.verify_native(salt, &auth),
        None => false,
    };
    if !verified {
        let e = SqlError::access_denied(&resp.user, host, !auth.is_empty());
        write_err(io, capability, &e).await.map_err(io_err)?;
        return Ok(None);
    }

    let info = ConnectionInfo {
        connection_id,
        user: resp.user.clone(),
        peer: peer.to_string(),
        database: resp.database.clone(),
        collation: resp.collation,
        capability,
        tls_cipher: io.get_ref().cipher(),
    };
    let mut handler = match factory.open_session(&info) {
        Ok(h) => h,
        Err(e) => {
            write_err(io, capability, &e).await.map_err(io_err)?;
            return Ok(None);
        }
    };
    if let Some(db) = &resp.database
        && let Err(e) = handler.use_db(db).await
    {
        write_err(io, capability, &e).await.map_err(io_err)?;
        return Ok(None);
    }

    let mut ok = vec![header::OK, 0, 0];
    ok.extend_from_slice(&handler.status_flags().to_le_bytes());
    ok.extend_from_slice(&[0, 0]);
    io.write_packet(&ok).await.map_err(io_err)?;
    io.flush().await.map_err(io_err)?;
    Ok(Some((handler, capability, resp.user)))
}

/// Format the last command body for `command dispatched failed` (Go `conn.go:2742-2768`),
/// redacting SQL when `redact_mode != "OFF"`.
fn format_cmd_sql_for_log(
    cmd: u8,
    body: &[u8],
    stmts: &HashMap<u32, StmtState>,
    redact_mode: &str,
) -> String {
    match cmd {
        command::QUERY | command::STMT_PREPARE => {
            let raw = String::from_utf8_lossy(body);
            // Strip at most one trailing NUL (Go `string(data[:len-1])` when NUL-terminated).
            let raw = raw.strip_suffix('\0').unwrap_or(&raw);
            let redacted = tidb_base::redact::string(redact_mode, raw);
            format_sql_for_conn_log(&redacted, QUERY_LOG_MAX_LEN)
        }
        command::STMT_EXECUTE | command::STMT_FETCH if body.len() >= 4 => {
            let id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
            if let Some(st) = stmts.get(&id) {
                let redacted = tidb_base::redact::string(redact_mode, &st.sql);
                format_sql_for_conn_log(&redacted, QUERY_LOG_MAX_LEN)
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

/// Host part of an `ip:port` peer address.
fn peer_host(peer: &str) -> &str {
    match peer.rsplit_once(':') {
        Some((host, _)) => host.trim_start_matches('[').trim_end_matches(']'),
        None => peer,
    }
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
}

impl<S: AsyncRead + AsyncWrite + Unpin, H: SessionHandler> Conn<S, H> {
    async fn command_loop(&mut self, connection_id: u64) {
        loop {
            self.io.reset_sequence();
            let data = match self.io.read_packet().await {
                Ok(d) => d,
                Err(PacketError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
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
                    warn!(connection_id, error = %e, "read packet failed, closing connection");
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
                    match write_err(&mut self.io, self.capability, &e).await {
                        Ok(()) => (Some(label), !fatal_close),
                        Err(e) => {
                            debug!(connection_id, error = %e, "write error packet failed");
                            (Some(label), false)
                        }
                    }
                }
                Err(ConnError::Fatal(e)) => {
                    debug!(connection_id, error = %e, "connection error");
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
                trace!(sql, "COM_QUERY");
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
        trace!(sql, "COM_STMT_PREPARE");
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
        assert_eq!(peer_host("local"), "local");
    }

    #[test]
    fn sql_text_trims_one_nul() {
        assert_eq!(sql_text(b"select 1\0").unwrap(), "select 1");
        assert_eq!(sql_text(b"select 1").unwrap(), "select 1");
        assert!(sql_text(&[0xff]).is_err());
    }
}
