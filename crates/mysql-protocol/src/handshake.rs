//! Connection-phase packets: `HandshakeV10`, `HandshakeResponse41` and `AuthSwitchRequest`
//! (TiDB `server/conn.go` and `server/internal/parse`).

use crate::auth::SALT_LEN;
use crate::codec::{Reader, put_nul_str};
use crate::constants::{capability::*, header};
use crate::error::{SqlError, code};

/// Fields of the initial handshake packet.
#[derive(Clone, Debug)]
pub struct InitialHandshake<'a> {
    pub server_version: &'a str,
    pub connection_id: u32,
    pub salt: &'a [u8; SALT_LEN],
    pub capability: u32,
    pub collation: u8,
    pub status: u16,
    pub auth_plugin: &'a str,
}

impl InitialHandshake<'_> {
    /// Encodes the `HandshakeV10` payload, byte-for-byte like TiDB's `writeInitialHandshake`.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(128);
        b.push(10);
        put_nul_str(&mut b, self.server_version.as_bytes());
        b.extend_from_slice(&self.connection_id.to_le_bytes());
        b.extend_from_slice(&self.salt[..8]);
        b.push(0);
        b.extend_from_slice(&(self.capability as u16).to_le_bytes());
        b.push(self.collation);
        b.extend_from_slice(&self.status.to_le_bytes());
        b.extend_from_slice(&((self.capability >> 16) as u16).to_le_bytes());
        b.push((SALT_LEN + 1) as u8);
        b.extend_from_slice(&[0; 10]);
        put_nul_str(&mut b, &self.salt[8..]);
        put_nul_str(&mut b, self.auth_plugin.as_bytes());
        b
    }
}

/// A parsed `HandshakeResponse41`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HandshakeResponse {
    /// Capability flags as sent by the client (not yet intersected with the server's).
    pub capability: u32,
    pub max_packet_size: u32,
    pub collation: u8,
    pub user: String,
    pub auth_response: Vec<u8>,
    pub database: Option<String>,
    pub auth_plugin: Option<String>,
    pub attrs: Vec<(String, String)>,
}

fn utf8(b: &[u8]) -> Result<String, SqlError> {
    String::from_utf8(b.to_vec()).map_err(|_| SqlError::malformed())
}

impl HandshakeResponse {
    /// Parses a handshake response. Pre-4.1 clients are rejected with
    /// `ER_NOT_SUPPORTED_AUTH_MODE` like TiDB. An SSLRequest (only the 32-byte header with
    /// `CLIENT_SSL`) is parsed as a response with an empty user; the caller rejects it since
    /// this server does not advertise TLS.
    pub fn parse(data: &[u8]) -> Result<Self, SqlError> {
        if data.len() < 2 {
            return Err(SqlError::malformed());
        }
        if u32::from(u16::from_le_bytes([data[0], data[1]])) & CLIENT_PROTOCOL_41 == 0 {
            return Err(SqlError::new(
                code::ER_NOT_SUPPORTED_AUTH_MODE,
                "Client does not support authentication protocol requested by server; consider upgrading MySQL client",
            ));
        }
        let mut r = Reader::new(data);
        let mut resp = HandshakeResponse {
            capability: r.u32()?,
            max_packet_size: r.u32()?,
            ..Default::default()
        };
        resp.collation = r.u8()?;
        r.skip(23)?;
        if resp.capability & CLIENT_SSL != 0 && r.is_empty() {
            return Ok(resp);
        }
        resp.user = utf8(r.nul_str()?)?;

        let caps = resp.capability;
        if caps & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            // libmysqlclient sets this bit even though the server does not advertise it; TiDB
            // treats a leading 0x01 as "no auth data" in that case.
            if r.peek_u8() == Some(0x01) {
                r.skip(2)?;
            } else if let Some(auth) = r.lenenc_str()? {
                resp.auth_response = auth.to_vec();
            }
        } else if caps & CLIENT_SECURE_CONNECTION != 0 {
            let n = usize::from(r.u8()?);
            resp.auth_response = r.take(n)?.to_vec();
        } else {
            resp.auth_response = r.nul_str()?.to_vec();
        }

        if caps & CLIENT_CONNECT_WITH_DB != 0 && !r.is_empty() {
            let db = utf8(r.nul_str()?)?;
            if !db.is_empty() {
                resp.database = Some(db);
            }
        }

        if caps & CLIENT_PLUGIN_AUTH != 0 && !r.is_empty() {
            // Some clients omit the trailing NUL.
            let rest = r.peek_rest();
            let plugin = match rest.iter().position(|&b| b == 0) {
                Some(_) => r.nul_str()?,
                None => r.rest(),
            };
            if !plugin.is_empty() {
                resp.auth_plugin = Some(utf8(plugin)?);
            }
        }

        if caps & CLIENT_CONNECT_ATTRS != 0 && !r.is_empty() {
            // Attributes are informational; ignore malformed ones like TiDB does.
            if let Ok(attrs) = parse_attrs(&mut r) {
                resp.attrs = attrs;
            }
        }
        Ok(resp)
    }
}

fn parse_attrs(r: &mut Reader<'_>) -> Result<Vec<(String, String)>, SqlError> {
    let Some(len) = r.lenenc_int()? else {
        return Ok(Vec::new());
    };
    let len = usize::try_from(len).map_err(|_| SqlError::malformed())?;
    let mut ar = Reader::new(r.take(len)?);
    let mut attrs = Vec::new();
    while !ar.is_empty() {
        let k = ar.lenenc_str()?.unwrap_or_default();
        let v = ar.lenenc_str()?.unwrap_or_default();
        attrs.push((
            String::from_utf8_lossy(k).into_owned(),
            String::from_utf8_lossy(v).into_owned(),
        ));
    }
    Ok(attrs)
}

/// `AuthSwitchRequest`: `0xfe`, plugin name, NUL, plugin data (the salt), NUL.
pub fn auth_switch_request(plugin: &str, salt: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(2 + plugin.len() + salt.len() + 1);
    b.push(header::AUTH_SWITCH_REQUEST);
    put_nul_str(&mut b, plugin.as_bytes());
    put_nul_str(&mut b, salt);
    b
}

/// Encodes a `HandshakeResponse41` (client side; used by tests and tools).
pub fn encode_handshake_response(
    capability: u32,
    user: &str,
    auth_response: &[u8],
    database: Option<&str>,
    auth_plugin: &str,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&capability.to_le_bytes());
    b.extend_from_slice(&(16u32 << 20).to_le_bytes());
    b.push(46);
    b.extend_from_slice(&[0; 23]);
    put_nul_str(&mut b, user.as_bytes());
    b.push(auth_response.len() as u8);
    b.extend_from_slice(auth_response);
    if capability & CLIENT_CONNECT_WITH_DB != 0 {
        put_nul_str(&mut b, database.unwrap_or_default().as_bytes());
    }
    if capability & CLIENT_PLUGIN_AUTH != 0 {
        put_nul_str(&mut b, auth_plugin.as_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{auth_plugin, status};

    #[test]
    fn initial_handshake_matches_tidb_layout() {
        // Mirrors TiDB's TestInitialHandshake (pkg/server/conn_test.go).
        let salt: [u8; 20] = std::array::from_fn(|i| i as u8 + 1);
        let version = "8.0.11-TiDB-v8.4.0-this-is-a-placeholder";
        let hs = InitialHandshake {
            server_version: version,
            connection_id: 1,
            salt: &salt,
            capability: TIDB_DEFAULT,
            collation: 46,
            status: status::AUTOCOMMIT,
            auth_plugin: auth_plugin::MYSQL_NATIVE_PASSWORD,
        };
        let mut expected = vec![0x0a];
        expected.extend_from_slice(version.as_bytes());
        expected.push(0);
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 0]);
        expected.extend_from_slice(&((TIDB_DEFAULT & 0xffff) as u16).to_le_bytes());
        expected.push(46);
        expected.extend_from_slice(&status::AUTOCOMMIT.to_le_bytes());
        expected.extend_from_slice(&((TIDB_DEFAULT >> 16) as u16).to_le_bytes());
        expected.push(0x15);
        expected.extend_from_slice(&[0; 10]);
        expected.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 0]);
        expected.extend_from_slice(b"mysql_native_password\0");
        assert_eq!(hs.encode(), expected);
    }

    #[test]
    fn tidb_default_capability_value() {
        // defaultCapability in pkg/server/server.go, spelled out.
        assert_eq!(TIDB_DEFAULT, 0x051b_a6af);
        assert_eq!(
            SERVER_DEFAULT & (CLIENT_COMPRESS | CLIENT_SSL | CLIENT_MULTI_STATEMENTS),
            0
        );
        assert_ne!(SERVER_DEFAULT & CLIENT_DEPRECATE_EOF, 0);
    }

    #[test]
    fn parse_handshake_response_roundtrip() {
        let caps = CLIENT_PROTOCOL_41
            | CLIENT_SECURE_CONNECTION
            | CLIENT_PLUGIN_AUTH
            | CLIENT_CONNECT_WITH_DB;
        let b = encode_handshake_response(
            caps,
            "root",
            &[1; 20],
            Some("test"),
            "mysql_native_password",
        );
        let r = HandshakeResponse::parse(&b).unwrap();
        assert_eq!(r.user, "root");
        assert_eq!(r.auth_response, vec![1; 20]);
        assert_eq!(r.database.as_deref(), Some("test"));
        assert_eq!(r.auth_plugin.as_deref(), Some("mysql_native_password"));
        assert_eq!(r.collation, 46);
    }

    #[test]
    fn parse_mysql80_client_fixture() {
        // Captured from a MySQL 8.0 client; TiDB's TestAuthSwitchRequest
        // (pkg/server/internal/parse/handshake_test.go).
        #[rustfmt::skip]
        let data: &[u8] = &[
            0x85, 0xa6, 0xff, 0x1, 0x0, 0x0, 0x0, 0x1, 0x21, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
            0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x72, 0x6f,
            0x6f, 0x74, 0x0, 0x0, 0x63, 0x61, 0x63, 0x68, 0x69, 0x6e, 0x67, 0x5f, 0x73, 0x68, 0x61,
            0x32, 0x5f, 0x70, 0x61, 0x73, 0x73, 0x77, 0x6f, 0x72, 0x64, 0x0, 0x79, 0x4, 0x5f, 0x70,
            0x69, 0x64, 0x5, 0x37, 0x37, 0x30, 0x38, 0x36, 0x9, 0x5f, 0x70, 0x6c, 0x61, 0x74, 0x66,
            0x6f, 0x72, 0x6d, 0x6, 0x78, 0x38, 0x36, 0x5f, 0x36, 0x34, 0x3, 0x5f, 0x6f, 0x73, 0x5,
            0x4c, 0x69, 0x6e, 0x75, 0x78, 0xc, 0x5f, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74, 0x5f, 0x6e,
            0x61, 0x6d, 0x65, 0x8, 0x6c, 0x69, 0x62, 0x6d, 0x79, 0x73, 0x71, 0x6c, 0x7, 0x6f, 0x73,
            0x5f, 0x75, 0x73, 0x65, 0x72, 0xa, 0x6e, 0x75, 0x6c, 0x6c, 0x6e, 0x6f, 0x74, 0x6e, 0x69,
            0x6c, 0xf, 0x5f, 0x63, 0x6c, 0x69, 0x65, 0x6e, 0x74, 0x5f, 0x76, 0x65, 0x72, 0x73, 0x69,
            0x6f, 0x6e, 0x6, 0x38, 0x2e, 0x30, 0x2e, 0x32, 0x31, 0xc, 0x70, 0x72, 0x6f, 0x67, 0x72,
            0x61, 0x6d, 0x5f, 0x6e, 0x61, 0x6d, 0x65, 0x5, 0x6d, 0x79, 0x73, 0x71, 0x6c,
        ];
        let r = HandshakeResponse::parse(data).unwrap();
        assert_ne!(r.capability & CLIENT_DEPRECATE_EOF, 0);
        assert_ne!(r.capability & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, 0);
        assert_eq!(r.collation, 0x21);
        assert_eq!(r.user, "root");
        assert!(r.auth_response.is_empty());
        assert_eq!(r.database, None);
        assert_eq!(r.auth_plugin.as_deref(), Some("caching_sha2_password"));
        assert_eq!(r.attrs.len(), 7);
        assert_eq!(r.attrs[0], ("_pid".to_string(), "77086".to_string()));
        assert_eq!(
            r.attrs[6],
            ("program_name".to_string(), "mysql".to_string())
        );
    }

    #[test]
    fn parse_lenenc_no_auth_marker() {
        let caps = CLIENT_PROTOCOL_41 | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;
        let mut b = Vec::new();
        b.extend_from_slice(&caps.to_le_bytes());
        b.extend_from_slice(&[0; 4 + 1 + 23]);
        b.extend_from_slice(b"root\0");
        b.extend_from_slice(&[0x01, 0x00]);
        let r = HandshakeResponse::parse(&b).unwrap();
        assert!(r.auth_response.is_empty());
    }

    #[test]
    fn parse_rejects_old_and_malformed() {
        assert_eq!(
            HandshakeResponse::parse(&[0, 0, 0, 0]).unwrap_err().code,
            code::ER_NOT_SUPPORTED_AUTH_MODE
        );
        assert!(HandshakeResponse::parse(&[0x00, 0x02]).is_err());
        let caps = CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION;
        let mut b = encode_handshake_response(caps, "root", &[1; 20], None, "");
        b.truncate(b.len() - 5);
        assert!(HandshakeResponse::parse(&b).is_err());
    }

    #[test]
    fn auth_switch_layout() {
        let salt = [b'a'; 20];
        let b = auth_switch_request("mysql_native_password", &salt);
        assert_eq!(b[0], 0xfe);
        assert_eq!(&b[1..23], b"mysql_native_password\0");
        assert_eq!(&b[23..43], &salt);
        assert_eq!(b[43], 0);
        assert_eq!(b.len(), 44);
    }
}
