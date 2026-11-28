//! Wire-protocol handshake and `COM_INIT_DB` tests backed by `tidb_privilege::Handle` and
//! `poc_session::privilege::authenticate_user_record`.
//!
//! Runs completely offline against a loopback `tidb_mysql_protocol::Server` without requiring PD
//! or TiKV.

use std::net::SocketAddr;
use std::sync::Arc;

use poc_session::privilege::authenticate_user_record;
use tidb_base::mysql::{PrivilegeType, SelectPriv};
use tidb_mysql_protocol::constants::{auth_plugin, capability::*, command};
use tidb_mysql_protocol::packet::PacketIo;
use tidb_mysql_protocol::{
    AccountAuth, ConnectionInfo, Credential, Param, PreparedInfo, Response, Server, ServerConfig,
    SessionFactory, SessionHandler, SqlError,
};
use tidb_privilege::{
    DbRecord, GlobalPrivRecord, Handle, MySQLPrivilege, PrivilegeManager, UserPrivileges,
    UserRecord,
};
use tokio::net::{TcpListener, TcpStream};

const PASSWORD: &str = "p4ssword!";
const BASE_CAPS: u32 =
    CLIENT_PROTOCOL_41 | CLIENT_SECURE_CONNECTION | CLIENT_PLUGIN_AUTH | CLIENT_LONG_PASSWORD;

fn native_hash(password: &str) -> String {
    let cred = Credential::native_from_password(password);
    match cred {
        Credential::NativePassword(Some(hash)) => {
            let mut out = String::with_capacity(41);
            out.push('*');
            for b in hash {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "{b:02X}");
            }
            out
        }
        Credential::NativePassword(None) => String::new(),
        Credential::CachingSha2Password(_) => {
            unreachable!("native_from_password returns NativePassword")
        }
    }
}

fn sha2_hash(password: &str) -> String {
    Credential::caching_sha2_from_password(password)
        .to_caching_sha2_authentication_string()
        .unwrap()
}

struct PrivTestFactory {
    handle: Arc<Handle>,
}

struct PrivTestSession {
    auth_user: String,
    auth_host: String,
    pm: UserPrivileges,
}

impl SessionFactory for PrivTestFactory {
    type Session = PrivTestSession;

    async fn lookup_user(&self, user: &str, host: &str) -> Option<Credential> {
        self.lookup_account(user, host).await.map(|a| a.credential)
    }

    async fn lookup_account(&self, user: &str, host: &str) -> Option<AccountAuth> {
        authenticate_user_record(&self.handle.get(), user, host)
    }

    fn open_session(&self, info: &ConnectionInfo) -> Result<PrivTestSession, SqlError> {
        let (Some(au), Some(ah)) = (&info.auth_user, &info.auth_host) else {
            return Err(SqlError::access_denied(&info.user, "127.0.0.1", false));
        };
        let pm = UserPrivileges::new(au.clone(), ah.clone(), Arc::clone(&self.handle));
        Ok(PrivTestSession {
            auth_user: au.clone(),
            auth_host: ah.clone(),
            pm,
        })
    }
}

impl SessionHandler for PrivTestSession {
    async fn query(&mut self, _sql: &str) -> Result<Response, SqlError> {
        Ok(Response::ok())
    }

    async fn prepare(&mut self, _stmt_id: u32, _sql: &str) -> Result<PreparedInfo, SqlError> {
        Err(SqlError::not_supported("prepare"))
    }

    async fn execute(&mut self, _stmt_id: u32, _params: Vec<Param>) -> Result<Response, SqlError> {
        Err(SqlError::not_supported("execute"))
    }

    fn close(&mut self, _stmt_id: u32) {}

    async fn use_db(&mut self, db: &str) -> Result<(), SqlError> {
        if !self.pm.db_is_visible(&[], db) {
            return Err(SqlError::db_access_denied(
                &self.auth_user,
                &self.auth_host,
                db,
            ));
        }
        Ok(())
    }
}

fn parse_err_packet(pkt: &[u8]) -> (u16, String, String) {
    assert_eq!(pkt[0], 0xff, "expected ERR packet, got {pkt:?}");
    let code = u16::from_le_bytes([pkt[1], pkt[2]]);
    assert_eq!(pkt[3], b'#', "expected SQLSTATE marker in {pkt:?}");
    let state = String::from_utf8(pkt[4..9].to_vec()).unwrap();
    let msg = String::from_utf8(pkt[9..].to_vec()).unwrap();
    (code, state, msg)
}

async fn raw_handshake(
    addr: SocketAddr,
    user: &str,
    password: &str,
    db: Option<&str>,
) -> (PacketIo<TcpStream>, Vec<u8>) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let mut io = PacketIo::new(stream, 1 << 20);
    let hs = io.read_packet().await.unwrap();
    assert_eq!(hs[0], 10);
    let ver_end = hs.iter().position(|&b| b == 0).unwrap();
    let p = ver_end + 1 + 4;
    let mut salt = hs[p..p + 8].to_vec();
    let p2 = p + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
    salt.extend_from_slice(&hs[p2..p2 + 12]);
    let scramble = tidb_mysql_protocol::auth::native_password_scramble(password.as_bytes(), &salt);
    let caps = if db.is_some() {
        BASE_CAPS | CLIENT_CONNECT_WITH_DB
    } else {
        BASE_CAPS
    };
    let resp = tidb_mysql_protocol::handshake::encode_handshake_response(
        caps,
        user,
        &scramble,
        db,
        auth_plugin::MYSQL_NATIVE_PASSWORD,
    );
    io.write_packet(&resp).await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    (io, reply)
}

fn build_fixture() -> Arc<Handle> {
    let pw = native_hash(PASSWORD);
    let mut snap = MySQLPrivilege::default();

    // 1. Unlocked account with SELECT on `test` only.
    snap.push_user(
        UserRecord::new("127.0.%", "u_ok", PrivilegeType(0))
            .with_auth_string(pw.clone())
            .with_auth_plugin("mysql_native_password"),
    );
    snap.push_db(DbRecord::new("u_ok", "127.0.%", "test", SelectPriv));

    // 2. Locked account.
    snap.push_user(
        UserRecord::new("127.0.%", "u_locked", PrivilegeType(0))
            .with_auth_string(pw.clone())
            .with_account_locked(true),
    );

    // 3. Unsupported non-native plugin accounts + malformed caching_sha2 hash (fail closed),
    //    plus valid empty-password & non-empty caching_sha2_password accounts.
    for (name, plugin, auth_str) in [
        ("u_token", "tidb_auth_token", ""),
        ("u_socket", "auth_socket", ""),
        ("u_ldap", "authentication_ldap_simple", ""),
        ("u_sm3_empty", "tidb_sm3_password", ""),
        ("u_sha2_malformed", "caching_sha2_password", "$A$005$broken"),
    ] {
        snap.push_user(
            UserRecord::new("%", name, SelectPriv)
                .with_auth_string(auth_str)
                .with_auth_plugin(plugin),
        );
    }
    snap.push_user(
        UserRecord::new("%", "u_sha2_empty", SelectPriv)
            .with_auth_string("")
            .with_auth_plugin("caching_sha2_password"),
    );
    snap.push_user(
        UserRecord::new("%", "u_sha2_empty_expired", SelectPriv)
            .with_auth_string("")
            .with_auth_plugin("caching_sha2_password")
            .with_password_expired(true),
    );
    snap.push_user(
        UserRecord::new("%", "u_sha2_ok", SelectPriv)
            .with_auth_string(sha2_hash(PASSWORD))
            .with_auth_plugin("caching_sha2_password"),
    );

    // 4. Restricted login accounts: SSL required, broken global_priv, expired, lifetime, lockout.
    snap.push_user(UserRecord::new("%", "u_ssl", SelectPriv).with_auth_string(pw.clone()));
    snap.push_global_priv(GlobalPrivRecord::new("u_ssl", "%", true, false));

    snap.push_user(UserRecord::new("%", "u_broken_gp", SelectPriv).with_auth_string(pw.clone()));
    snap.push_global_priv(GlobalPrivRecord::new("u_broken_gp", "%", false, true));

    let local_loc = tidb_base::gostd::time::local();
    let now = tidb_base::gostd::time::now().in_loc(&local_loc);

    snap.push_user(
        UserRecord::new("%", "u_expired", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_expired(true),
    );
    snap.push_user(
        UserRecord::new("%", "u_expired_locked", SelectPriv)
            .with_auth_string(pw.clone())
            .with_account_locked(true)
            .with_password_expired(true),
    );
    snap.push_user(
        UserRecord::new("%", "u_lifetime_fresh", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_lifetime(Some(30))
            .with_password_last_changed(Some(now.add_date(0, 0, -5))),
    );
    snap.push_user(
        UserRecord::new("%", "u_lifetime_expired", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_lifetime(Some(30))
            .with_password_last_changed(Some(now.add_date(0, 0, -31))),
    );
    snap.push_user(
        UserRecord::new("%", "u_lifetime_null_ts", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_lifetime(Some(30)),
    );
    snap.push_user(
        UserRecord::new("%", "u_lockout", SelectPriv)
            .with_auth_string(pw)
            .with_failed_login_policy(true),
    );

    snap.sort_all();
    Arc::new(Handle::new(snap))
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_handshake_enforces_locked_plugins_restrictions_and_use_db() {
    let handle = build_fixture();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        Server::new(ServerConfig::default(), PrivTestFactory { handle })
            .serve(listener, std::future::pending())
            .await
            .unwrap();
    });

    // Locked account + right password -> 3118 (HY000) with exact Go message.
    let (_, reply) = raw_handshake(addr, "u_locked", PASSWORD, None).await;
    let (code, state, msg) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (3118, "HY000"));
    assert_eq!(
        msg,
        "Access denied for user 'u_locked'@'127.0.0.1'. Account is locked."
    );

    // Locked account + wrong password -> 1045 (28000).
    let (_, reply) = raw_handshake(addr, "u_locked", "wrong", None).await;
    let (code, state, _) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1045, "28000"));

    // Locked AND expired account + right password -> 3118 (locked precedes expired, matching Go).
    let (_, reply) = raw_handshake(addr, "u_expired_locked", PASSWORD, None).await;
    let (code, state, msg) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (3118, "HY000"));
    assert_eq!(
        msg,
        "Access denied for user 'u_expired_locked'@'127.0.0.1'. Account is locked."
    );

    // Expired accounts + right password -> 1862 (HY000) with exact Go message (also precedes use_db 1044).
    const EXPIRED_MSG: &str = "Your password has expired. To log in you must change it using a client that supports expired passwords.";
    for (user, db) in [
        ("u_expired", None),
        ("u_expired", Some("forbidden")),
        ("u_lifetime_expired", None),
        ("u_lifetime_null_ts", None),
    ] {
        let (_, reply) = raw_handshake(addr, user, PASSWORD, db).await;
        let (code, state, msg) = parse_err_packet(&reply);
        assert_eq!(
            (code, state.as_str()),
            (1862, "HY000"),
            "expired user {user} (db={db:?}) must return 1862: {msg}"
        );
        assert_eq!(msg, EXPIRED_MSG);
    }

    // Expired account + wrong password -> 1045 (28000) (password check precedes 1862).
    let (_, reply) = raw_handshake(addr, "u_expired", "wrong", None).await;
    let (code, state, _) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1045, "28000"));

    // Expired passwordless caching_sha2_password account -> 1862 on empty auth, 1045 on wrong auth.
    let (mut io, reply) = raw_handshake(addr, "u_sha2_empty_expired", "", None).await;
    assert_eq!(reply[0], 0xfe);
    io.write_packet(b"").await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    let (code, state, msg) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1862, "HY000"));
    assert_eq!(msg, EXPIRED_MSG);

    let (mut io, reply) = raw_handshake(addr, "u_sha2_empty_expired", "wrong", None).await;
    assert_eq!(reply[0], 0xfe);
    io.write_packet(b"wrong").await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    let (code, state, _) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1045, "28000"));

    // Unsupported plugins, malformed caching_sha2 hash, & unported restrictions -> fail closed with 1045 (28000).
    for (user, pw) in [
        ("u_token", ""),
        ("u_socket", ""),
        ("u_ldap", ""),
        ("u_sm3_empty", ""),
        ("u_sha2_malformed", PASSWORD),
        ("u_ssl", PASSWORD),
        ("u_broken_gp", PASSWORD),
        ("u_lockout", PASSWORD),
    ] {
        let (_, reply) = raw_handshake(addr, user, pw, None).await;
        let (code, state, msg) = parse_err_packet(&reply);
        assert_eq!(
            (code, state.as_str()),
            (1045, "28000"),
            "user {user} must fail closed: {msg}"
        );
    }

    // Passwordless caching_sha2_password switches plugin, succeeds on empty auth, rejects non-empty.
    let (mut io, reply) = raw_handshake(addr, "u_sha2_empty", "", None).await;
    assert_eq!(
        reply[0], 0xfe,
        "expected AuthSwitchRequest for caching_sha2_password, got {reply:?}"
    );
    assert!(
        reply[1..].starts_with(b"caching_sha2_password\0"),
        "expected caching_sha2_password plugin in AuthSwitchRequest, got {reply:?}"
    );
    io.write_packet(b"").await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    assert_eq!(
        reply[0], 0x00,
        "passwordless caching_sha2_password must succeed: {reply:?}"
    );

    let (mut io, reply) = raw_handshake(addr, "u_sha2_empty", "wrong", None).await;
    assert_eq!(reply[0], 0xfe);
    io.write_packet(b"wrong").await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    let (code, state, _) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1045, "28000"));

    // Non-empty caching_sha2_password over plain TCP must never accept cleartext password (1045).
    let (mut io, reply) = raw_handshake(addr, "u_sha2_ok", PASSWORD, None).await;
    assert_eq!(reply[0], 0xfe);
    io.write_packet(b"initial-non-empty-auth").await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    assert_eq!(
        reply,
        vec![0x01, 0x04],
        "expected AuthMoreData perform-full-auth, got {reply:?}"
    );
    let mut pw_pkt = PASSWORD.as_bytes().to_vec();
    pw_pkt.push(0);
    io.write_packet(&pw_pkt).await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    let (code, state, _) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1045, "28000"));

    // Account with explicit 30-day lifetime rotated 5 days ago -> OK.
    let (_, reply) = raw_handshake(addr, "u_lifetime_fresh", PASSWORD, None).await;
    assert_eq!(
        reply[0], 0x00,
        "fresh lifetime account must succeed: {reply:?}"
    );
    // Handshake with unauthorized default database -> 1044 (42000).
    let (_, reply) = raw_handshake(addr, "u_ok", PASSWORD, Some("secret")).await;
    let (code, state, msg) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1044, "42000"));
    assert_eq!(
        msg,
        "Access denied for user 'u_ok'@'127.0.%' to database 'secret'"
    );

    // Handshake with authorized default database -> OK, then COM_INIT_DB to unauthorized db -> 1044.
    let (mut io, reply) = raw_handshake(addr, "u_ok", PASSWORD, Some("test")).await;
    assert_eq!(reply[0], 0x00, "expected OK packet, got {reply:?}");

    io.reset_sequence();
    let mut pkt = vec![command::INIT_DB];
    pkt.extend_from_slice(b"secret");
    io.write_packet(&pkt).await.unwrap();
    io.flush().await.unwrap();
    let reply = io.read_packet().await.unwrap();
    let (code, state, msg) = parse_err_packet(&reply);
    assert_eq!((code, state.as_str()), (1044, "42000"));
    assert_eq!(
        msg,
        "Access denied for user 'u_ok'@'127.0.%' to database 'secret'"
    );

    // When `@@global.default_password_lifetime = 90`:
    // - fresh account with `password_lifetime IS NULL` logs in (OK),
    // - expired (100d old) or NULL-timestamp account with `password_lifetime IS NULL` returns 1862,
    // - 100d old `PASSWORD EXPIRE NEVER` (`Some(0)`) still logs in (OK).
    let local_loc = tidb_base::gostd::time::local();
    let now = tidb_base::gostd::time::now().in_loc(&local_loc);
    let pw = native_hash(PASSWORD);
    let mut snap2 = MySQLPrivilege::default();
    snap2.set_default_password_lifetime(90);
    snap2.push_user(
        UserRecord::new("%", "u_default_fresh", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_last_changed(Some(now.add_date(0, 0, -10))),
    );
    snap2.push_user(
        UserRecord::new("%", "u_default_expired", SelectPriv)
            .with_auth_string(pw.clone())
            .with_password_last_changed(Some(now.add_date(0, 0, -100))),
    );
    snap2.push_user(UserRecord::new("%", "u_null_ts", SelectPriv).with_auth_string(pw.clone()));
    snap2.push_user(
        UserRecord::new("%", "u_never_lt", SelectPriv)
            .with_auth_string(pw)
            .with_password_lifetime(Some(0))
            .with_password_last_changed(Some(now.add_date(0, 0, -100))),
    );
    snap2.sort_all();
    let handle2 = Arc::new(Handle::new(snap2));
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr2 = listener2.local_addr().unwrap();
    tokio::spawn(async move {
        Server::new(ServerConfig::default(), PrivTestFactory { handle: handle2 })
            .serve(listener2, std::future::pending())
            .await
            .unwrap();
    });
    let (_, reply) = raw_handshake(addr2, "u_default_fresh", PASSWORD, None).await;
    assert_eq!(
        reply[0], 0x00,
        "fresh default-lifetime account must succeed: {reply:?}"
    );
    for expired in ["u_default_expired", "u_null_ts"] {
        let (_, reply) = raw_handshake(addr2, expired, PASSWORD, None).await;
        let (code, state, msg) = parse_err_packet(&reply);
        assert_eq!(
            (code, state.as_str()),
            (1862, "HY000"),
            "{expired} must return 1862: {msg}"
        );
        assert_eq!(msg, EXPIRED_MSG);
    }
    let (_, reply) = raw_handshake(addr2, "u_never_lt", PASSWORD, None).await;
    assert_eq!(
        reply[0], 0x00,
        "PASSWORD EXPIRE NEVER must succeed: {reply:?}"
    );
}
