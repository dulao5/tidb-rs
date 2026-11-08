//! go-file: pkg/sessionctx/variable/statusvar.go
//!
//! Status variables (SHOW STATUS): providers register themselves; the default provider
//! reports the TLS state of the connection and the connection-attribute counters.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;

use parking_lot::RwLock;

use super::session::SessionVars;
use super::tidb_vars::{ConnectAttrsLongestSeen, ConnectAttrsLost};
use super::variable::{ScopeFlag, ScopeGlobal, ScopeSession};

/// A status value (Go `any`): the kinds the providers produce.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusValue {
    /// A string.
    Str(String),
    /// An int.
    Int(i64),
}

impl std::fmt::Display for StatusValue {
    // go-std: fmt.Sprint of the value L1
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatusValue::Str(s) => f.write_str(s),
            StatusValue::Int(i) => write!(f, "{i}"),
        }
    }
}

/// StatusVal is the value of the corresponding status variable.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusVal {
    /// Scope of the status variable.
    pub scope: ScopeFlag,
    /// The value.
    pub value: StatusValue,
}

/// Statistics is the interface of statistics.
pub trait Statistics: Send + Sync {
    /// GetScope gets the status variables scope.
    // go-part: Statistics.GetScope (interface method)
    fn get_scope(&self, status: &str) -> ScopeFlag;
    /// Stats returns the statistics status variables.
    // go-part: Statistics.Stats (interface method)
    fn stats(
        &self,
        vars: Option<&SessionVars>,
    ) -> Result<HashMap<String, StatusValue>, tidb_base::Error>;
}

#[allow(non_upper_case_globals)]
static statisticsList: LazyLock<RwLock<Vec<Arc<dyn Statistics>>>> =
    LazyLock::new(|| RwLock::new(vec![Arc::new(DefaultStatusStat) as Arc<dyn Statistics>]));

/// RegisterStatistics registers statistics.
// go: RegisterStatistics L1
pub fn register_statistics(s: Arc<dyn Statistics>) {
    statisticsList.write().push(s);
}

/// UnregisterStatistics unregisters statistics.
// go: UnregisterStatistics L1
pub fn unregister_statistics(s: &Arc<dyn Statistics>) {
    let mut list = statisticsList.write();
    let mut idx: Option<usize> = None;
    for (i, x) in list.iter().enumerate() {
        if Arc::ptr_eq(x, s) {
            idx = Some(i);
        }
    }
    let Some(idx) = idx else { return };
    list.swap_remove(idx);
}

/// GetStatusVars gets registered statistics status variables.
// go: GetStatusVars L1
pub fn get_status_vars(
    vars: Option<&SessionVars>,
) -> Result<HashMap<String, StatusVal>, tidb_base::Error> {
    let mut status_vars = HashMap::new();
    for statistics in statisticsList.read().iter() {
        let vals = statistics.stats(vars)?;
        for (name, val) in vals {
            let scope = statistics.get_scope(&name);
            status_vars.insert(name, StatusVal { value: val, scope });
        }
    }
    Ok(status_vars)
}

/// tlsCiphers / util.TLSCipher2String: the supported ciphers in Go's order, with their
/// OpenSSL names.
#[allow(non_upper_case_globals)]
static tlsCiphers: &[&str] = &[
    "RC4-SHA",
    "DES-CBC3-SHA",
    "AES128-SHA",
    "AES256-SHA",
    "AES128-SHA256",
    "AES128-GCM-SHA256",
    "AES256-GCM-SHA384",
    "ECDHE-ECDSA-RC4-SHA",
    "ECDHE-ECDSA-AES128-SHA",
    "ECDHE-ECDSA-AES256-SHA",
    "ECDHE-RSA-RC4-SHA",
    "ECDHE-RSA-DES-CBC3-SHA",
    "ECDHE-RSA-AES128-SHA",
    "ECDHE-RSA-AES256-SHA",
    "ECDHE-ECDSA-AES128-SHA256",
    "ECDHE-RSA-AES128-SHA256",
    "ECDHE-RSA-AES128-GCM-SHA256",
    "ECDHE-ECDSA-AES128-GCM-SHA256",
    "ECDHE-RSA-AES256-GCM-SHA384",
    "ECDHE-ECDSA-AES256-GCM-SHA384",
    "ECDHE-RSA-CHACHA20-POLY1305",
    "ECDHE-ECDSA-CHACHA20-POLY1305",
    "TLS_AES_128_GCM_SHA256",
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
];

/// tlsSupportedCiphers: every cipher name followed by ':' (statusvar.go init).
// go: pkg/sessionctx/variable/statusvar.go init L2 (cipher list built lazily; the default provider is registered in statisticsList's initializer)
#[allow(non_upper_case_globals)]
static tlsSupportedCiphers: LazyLock<String> = LazyLock::new(|| {
    let mut b = String::new();
    for v in tlsCiphers {
        b.push_str(v);
        b.push(':');
    }
    b
});

/// defaultStatus: the status variables of the default provider and their initial values.
// glue: defaultStatus (Go package variable)
fn default_status() -> Vec<(&'static str, ScopeFlag, StatusValue)> {
    let gs = ScopeFlag(ScopeGlobal.0 | ScopeSession.0);
    vec![
        ("Ssl_cipher", gs, StatusValue::Str(String::new())),
        ("Ssl_cipher_list", gs, StatusValue::Str(String::new())),
        ("Ssl_verify_mode", gs, StatusValue::Int(0)),
        ("Ssl_version", gs, StatusValue::Str(String::new())),
        (
            "Performance_schema_session_connect_attrs_longest_seen",
            ScopeGlobal,
            StatusValue::Int(0),
        ),
        (
            "Performance_schema_session_connect_attrs_lost",
            ScopeGlobal,
            StatusValue::Int(0),
        ),
    ]
}

/// The default status provider.
pub struct DefaultStatusStat;

impl Statistics for DefaultStatusStat {
    // go: defaultStatusStat.GetScope L1
    fn get_scope(&self, status: &str) -> ScopeFlag {
        default_status()
            .into_iter()
            .find(|(n, _, _)| *n == status)
            .map(|(_, s, _)| s)
            .expect("known status variable")
    }

    // go: defaultStatusStat.Stats L2 (the TLS state of the D05.2 stub has no cipher / version: Go's zero values)
    fn stats(
        &self,
        vars: Option<&SessionVars>,
    ) -> Result<HashMap<String, StatusValue>, tidb_base::Error> {
        let mut status_vars: HashMap<String, StatusValue> = default_status()
            .into_iter()
            .map(|(n, _, v)| (n.to_owned(), v))
            .collect();
        status_vars.insert(
            "Performance_schema_session_connect_attrs_longest_seen".to_owned(),
            StatusValue::Int(ConnectAttrsLongestSeen.load(std::sync::atomic::Ordering::SeqCst)),
        );
        status_vars.insert(
            "Performance_schema_session_connect_attrs_lost".to_owned(),
            StatusValue::Int(ConnectAttrsLost.load(std::sync::atomic::Ordering::SeqCst)),
        );
        if let Some(vars) = vars
            && vars.tls_connection_state.is_some()
        {
            {
                // util.TLSCipher2String(CipherSuite 0) and tlsVersionString[Version 0]: unknown.
                status_vars.insert("Ssl_cipher".to_owned(), StatusValue::Str(String::new()));
                status_vars.insert(
                    "Ssl_cipher_list".to_owned(),
                    StatusValue::Str(tlsSupportedCiphers.clone()),
                );
                status_vars.insert("Ssl_verify_mode".to_owned(), StatusValue::Int(0x01 | 0x04));
                status_vars.insert(
                    "Ssl_version".to_owned(),
                    StatusValue::Str("unknown_tls_version".to_owned()),
                );
            }
        }
        Ok(status_vars)
    }
}
