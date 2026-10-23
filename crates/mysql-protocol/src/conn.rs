//! One client connection: handshake, authentication and the command loop
//! (TiDB `server/conn.go`, `server/conn_stmt.go`).

use std::collections::HashMap;
use std::io;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

use crate::auth::{Credential, generate_salt};
use crate::codec::{Reader, put_lenenc_int, put_lenenc_str};
use crate::constants::{auth_plugin, capability::*, collation, command, cursor_type, header};
use crate::error::{SqlError, code};
use crate::handler::{ConnectionInfo, OkInfo, Response, ResultSet, SessionFactory, SessionHandler};
use crate::handshake::{HandshakeResponse, InitialHandshake, auth_switch_request};
use crate::packet::{PacketError, PacketIo};
use crate::params::decode_params;
use crate::server::ServerConfig;
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
    config: &ServerConfig,
    factory: &F,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    let mut io = PacketIo::new(stream, config.max_allowed_packet);
    let mut conn = match handshake(&mut io, &peer, connection_id, config, factory).await {
        Ok(Some(handler_and_caps)) => {
            let (handler, capability) = handler_and_caps;
            Conn {
                io,
                capability,
                handler,
                stmts: HashMap::new(),
                next_stmt_id: 1,
                started: Instant::now(),
            }
        }
        Ok(None) => return,
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

/// Performs the connection phase. Returns `None` after an orderly rejection (an ERR packet
/// was sent), `Err` on I/O failure.
async fn handshake<S, F>(
    io: &mut PacketIo<S>,
    peer: &str,
    connection_id: u64,
    config: &ServerConfig,
    factory: &F,
) -> Result<Option<(F::Session, u32)>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: SessionFactory,
{
    let salt = generate_salt();
    let hs = InitialHandshake {
        server_version: &config.server_version,
        connection_id: connection_id as u32,
        salt: &salt,
        capability: config.capability,
        collation: collation::UTF8MB4_BIN as u8,
        status: crate::constants::status::AUTOCOMMIT,
        auth_plugin: &config.default_auth_plugin,
    };
    let io_err = |e: io::Error| e.to_string();
    io.write_packet(&hs.encode()).await.map_err(io_err)?;
    io.flush().await.map_err(io_err)?;

    let data = io.read_packet().await.map_err(|e| e.to_string())?;
    let resp = match HandshakeResponse::parse(&data) {
        Ok(r) => r,
        Err(e) => {
            write_err(io, CLIENT_PROTOCOL_41, &e)
                .await
                .map_err(io_err)?;
            return Ok(None);
        }
    };
    if resp.capability & CLIENT_SSL != 0 {
        // TLS is not advertised, so a conforming client never asks for it.
        let e = SqlError::not_supported("SSL");
        write_err(io, resp.capability, &e).await.map_err(io_err)?;
        return Ok(None);
    }
    let capability = resp.capability & config.capability;

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
            &salt,
        ))
        .await
        .map_err(io_err)?;
        io.flush().await.map_err(io_err)?;
        auth = io.read_packet().await.map_err(|e| e.to_string())?;
    }

    let host = peer_host(peer);
    let verified = match factory.lookup_user(&resp.user, host).await {
        Some(cred @ Credential::NativePassword(_)) => cred.verify_native(&salt, &auth),
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
    Ok(Some((handler, capability)))
}

/// Host part of an `ip:port` peer address.
fn peer_host(peer: &str) -> &str {
    match peer.rsplit_once(':') {
        Some((host, _)) => host.trim_start_matches('[').trim_end_matches(']'),
        None => peer,
    }
}

struct Conn<S, H> {
    io: PacketIo<S>,
    capability: u32,
    handler: H,
    stmts: HashMap<u32, StmtState>,
    next_stmt_id: u32,
    started: Instant,
}

impl<S: AsyncRead + AsyncWrite + Unpin, H: SessionHandler> Conn<S, H> {
    async fn command_loop(&mut self, connection_id: u64) {
        loop {
            self.io.reset_sequence();
            let data = match self.io.read_packet().await {
                Ok(d) => d,
                Err(PacketError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => return,
                Err(PacketError::TooLarge) => {
                    let e = SqlError::new(
                        code::ER_NET_PACKET_TOO_LARGE,
                        PacketError::TooLarge.to_string(),
                    );
                    let _ = write_err(&mut self.io, self.capability, &e).await;
                    return;
                }
                Err(e) => {
                    warn!(connection_id, error = %e, "read packet failed, closing connection");
                    return;
                }
            };
            let result = match data.split_first() {
                None => Err(ConnError::Sql(SqlError::malformed())),
                Some((&command::QUIT, _)) => return,
                Some((&cmd, body)) => self.dispatch(cmd, body).await,
            };
            match result {
                Ok(()) => {}
                Err(ConnError::Sql(e)) => {
                    debug!(connection_id, error = %e, "command failed");
                    if let Err(e) = write_err(&mut self.io, self.capability, &e).await {
                        debug!(connection_id, error = %e, "write error packet failed");
                        return;
                    }
                }
                Err(ConnError::Fatal(e)) => {
                    debug!(connection_id, error = %e, "connection error");
                    return;
                }
            }
        }
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
                let resp = self.handler.query(sql).await?;
                self.write_response(resp, false).await
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
        let id = self.next_stmt_id;
        self.next_stmt_id = self.next_stmt_id.checked_add(1).unwrap_or(1);
        let info = self.handler.prepare(id, sql).await?;
        let param_count = info.param_count;
        self.stmts.insert(
            id,
            StmtState {
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
        if !info.columns.is_empty() {
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
        let params = if n > 0 {
            let null_bitmap = r.take(n.div_ceil(8))?;
            if r.u8()? == 1 {
                stmt.param_types = r.take(2 * n)?.to_vec();
            }
            let result =
                decode_params(n, &stmt.long_data, null_bitmap, &stmt.param_types, r.rest());
            // Long data is consumed by an execute, successful or not (TiDB resets the stmt).
            stmt.long_data.iter_mut().for_each(|d| *d = None);
            result?
        } else {
            Vec::new()
        };
        let resp = self.handler.execute(id, params).await?;
        self.write_response(resp, true).await
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
        }
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
