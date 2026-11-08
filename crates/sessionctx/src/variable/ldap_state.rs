//! go-file: pkg/privilege/privileges/ldap/ldap_common.go
//! go-file: pkg/privilege/privileges/ldap/sasl.go
//!
//! The configuration state of the LDAP authentication plugins (Go `ldap.LDAPSASLAuthImpl`
//! and `ldap.LDAPSimpleAuthImpl`), which the `authentication_ldap_*` system variables set and
//! read (D05.2 T5). Only the settings and their getters / setters are here: the
//! authentication itself (bind, search, SASL exchange) and the connection pool belong to the
//! privilege block (D04.1). `initializePool` rebuilds the vitess resource pool, which opens
//! connections lazily; it is runtime-only and has no counterpart until D04.1 (the setters keep
//! Go's "changed" checks so that D04.1 can hook the rebuild in at the same places).
//!
//! Go starts both impls from the zero value (`&ldapAuthImpl{}`): the system variables'
//! defaults reach them only when the global values are loaded (SetGlobal at start-up).

// Go names for the package-level impls (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::errors;

/// Go `ldapAuthImpl` (the settings; the pool and the CA pool are not modelled, see the
/// module docs).
#[derive(Default)]
struct LdapSettings {
    bind_base_dn: String,
    bind_root_dn: String,
    bind_root_pwd: String,
    search_attr: String,
    ldap_server_host: String,
    ldap_server_port: isize,
    enable_tls: bool,
    ca_path: String,
    init_capacity: isize,
    max_capacity: isize,
    /// `ldapSASLAuthImpl.saslAuthMethod` (unused by the simple impl).
    sasl_auth_method: String,
}

/// Go `*ldapAuthImpl` / `*ldapSASLAuthImpl` / `*ldapSimplAuthImpl` (a `sync.RWMutex` around
/// the settings).
pub struct LdapAuthImpl(Mutex<LdapSettings>);

/// Go `ldap.LDAPSASLAuthImpl`.
pub static LDAPSASLAuthImpl: LazyLock<LdapAuthImpl> =
    LazyLock::new(|| LdapAuthImpl(Mutex::new(LdapSettings::default())));
/// Go `ldap.LDAPSimpleAuthImpl`.
pub static LDAPSimpleAuthImpl: LazyLock<LdapAuthImpl> =
    LazyLock::new(|| LdapAuthImpl(Mutex::new(LdapSettings::default())));

impl LdapAuthImpl {
    /// Puts the impl back to Go's zero value (test support: the replay starts every record
    /// from the process state after init).
    // glue: test reset of the process-wide LDAP settings
    pub fn reset(&self) {
        *self.0.lock() = LdapSettings::default();
    }

    // go: ldapAuthImpl.initializePool L3
    fn initialize_pool(_s: &LdapSettings) {
        // Runtime-only (module docs): rebuilt by the privilege block (D04.1).
    }

    // go: ldapAuthImpl.initializeCAPool L1
    fn initialize_ca_pool(s: &LdapSettings) -> Result<(), Error> {
        if s.ca_path.is_empty() {
            return Ok(());
        }
        let ca_cert = match tidb_base::gostd::os::open_read_all(s.ca_path.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                // Go returns `errors.Wrapf(err, "read ca certificate at %s", caCert)` (formatting
                // the nil `caCert`, not the path; tidb-rs #64). The server writes
                // `errors.Cause(err)` to the client, so the client sees only the os error
                // "open <path>: <errno>"; the Wrapf text reaches Go's log only. M-ERR ports
                // the wrapper away: the client-visible error is returned.
                return Err(errors::new(e));
            }
        };
        // x509.CertPool.AppendCertsFromPEM: true when at least one PEM "CERTIFICATE" block
        // parses. The certificate parsing itself comes with D04.1's TLS support; until then
        // a file without such a block is rejected like Go.
        if !contains_pem_certificate(&ca_cert) {
            return Err(errors::new("fail to parse ca certificate"));
        }
        Ok(())
    }

    // go: ldapAuthImpl.SetBindBaseDN L1
    pub fn set_bind_base_dn(&self, v: &str) {
        self.0.lock().bind_base_dn = v.to_owned();
    }

    // go: ldapAuthImpl.SetBindRootDN L1
    pub fn set_bind_root_dn(&self, v: &str) {
        self.0.lock().bind_root_dn = v.to_owned();
    }

    // go: ldapAuthImpl.SetBindRootPW L1
    pub fn set_bind_root_pw(&self, v: &str) {
        self.0.lock().bind_root_pwd = v.to_owned();
    }

    // go: ldapAuthImpl.SetSearchAttr L1
    pub fn set_search_attr(&self, v: &str) {
        self.0.lock().search_attr = v.to_owned();
    }

    // go: ldapAuthImpl.SetLDAPServerHost L1
    pub fn set_ldap_server_host(&self, v: &str) {
        let mut s = self.0.lock();
        if v != s.ldap_server_host {
            s.ldap_server_host = v.to_owned();
            Self::initialize_pool(&s);
        }
    }

    // go: ldapAuthImpl.SetLDAPServerPort L1
    pub fn set_ldap_server_port(&self, v: isize) {
        let mut s = self.0.lock();
        if v != s.ldap_server_port {
            s.ldap_server_port = v;
            Self::initialize_pool(&s);
        }
    }

    // go: ldapAuthImpl.SetEnableTLS L1
    pub fn set_enable_tls(&self, v: bool) {
        let mut s = self.0.lock();
        if v != s.enable_tls {
            s.enable_tls = v;
            Self::initialize_pool(&s);
        }
    }

    // go: ldapAuthImpl.SetCAPath L1
    pub fn set_ca_path(&self, path: &str) -> Result<(), Error> {
        let mut s = self.0.lock();
        if path != s.ca_path {
            s.ca_path = path.to_owned();
            Self::initialize_ca_pool(&s)?;
        }
        Ok(())
    }

    // go: ldapAuthImpl.SetInitCapacity L1
    pub fn set_init_capacity(&self, v: isize) {
        let mut s = self.0.lock();
        if v != s.init_capacity {
            s.init_capacity = v;
            Self::initialize_pool(&s);
        }
    }

    // go: ldapAuthImpl.SetMaxCapacity L1
    pub fn set_max_capacity(&self, v: isize) {
        let mut s = self.0.lock();
        if v != s.max_capacity {
            s.max_capacity = v;
            Self::initialize_pool(&s);
        }
    }

    // go: ldapAuthImpl.GetBindBaseDN L1
    pub fn get_bind_base_dn(&self) -> String {
        self.0.lock().bind_base_dn.clone()
    }

    // go: ldapAuthImpl.GetBindRootDN L1
    pub fn get_bind_root_dn(&self) -> String {
        self.0.lock().bind_root_dn.clone()
    }

    // go: ldapAuthImpl.GetBindRootPW L1
    pub fn get_bind_root_pw(&self) -> String {
        self.0.lock().bind_root_pwd.clone()
    }

    // go: ldapAuthImpl.GetSearchAttr L1
    pub fn get_search_attr(&self) -> String {
        self.0.lock().search_attr.clone()
    }

    // go: ldapAuthImpl.GetLDAPServerHost L1
    pub fn get_ldap_server_host(&self) -> String {
        self.0.lock().ldap_server_host.clone()
    }

    // go: ldapAuthImpl.GetLDAPServerPort L1
    pub fn get_ldap_server_port(&self) -> isize {
        self.0.lock().ldap_server_port
    }

    // go: ldapAuthImpl.GetEnableTLS L1
    pub fn get_enable_tls(&self) -> bool {
        self.0.lock().enable_tls
    }

    // go: ldapAuthImpl.GetCAPath L1
    pub fn get_ca_path(&self) -> String {
        self.0.lock().ca_path.clone()
    }

    // go: ldapAuthImpl.GetInitCapacity L1
    pub fn get_init_capacity(&self) -> isize {
        self.0.lock().init_capacity
    }

    // go: ldapAuthImpl.GetMaxCapacity L1
    pub fn get_max_capacity(&self) -> isize {
        self.0.lock().max_capacity
    }

    // go: ldapSASLAuthImpl.SetSASLAuthMethod L1
    pub fn set_sasl_auth_method(&self, v: &str) {
        self.0.lock().sasl_auth_method = v.to_owned();
    }

    // go: ldapSASLAuthImpl.GetSASLAuthMethod L1
    pub fn get_sasl_auth_method(&self) -> String {
        self.0.lock().sasl_auth_method.clone()
    }
}

/// Whether `data` holds a PEM block of type CERTIFICATE (the first condition of
/// `AppendCertsFromPEM`; the DER parse of the block comes with D04.1).
// glue: pem.Decode loop of x509.CertPool.AppendCertsFromPEM (block type check only)
fn contains_pem_certificate(data: &[u8]) -> bool {
    let begin = b"-----BEGIN CERTIFICATE-----";
    let end = b"-----END CERTIFICATE-----";
    match data.windows(begin.len()).position(|w| w == begin) {
        Some(i) => data[i..].windows(end.len()).any(|w| w == end),
        None => false,
    }
}
