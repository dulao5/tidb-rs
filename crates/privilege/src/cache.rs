//! go-file: pkg/privilege/privileges/cache.go
//!
//! Faithful port of Go TiDB v8.5.8 `MySQLPrivilege` in-memory grant cache and verification
//! engine (`RequestVerification`, `RequestDynamicVerification`, `MatchColumns`, role graph
//! traversal, and reloadable `Handle`).

#![allow(non_upper_case_globals)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use arc_swap::ArcSwap;
use tidb_base::mysql::{
    AlterPriv, CreatePriv, CreateViewPriv, DeletePriv, DropPriv, ExecutePriv, GrantPriv, IndexPriv,
    InsertPriv, PrivilegeType, ProcessPriv, ReferencesPriv, SelectPriv, ShowDBPriv, ShowViewPriv,
    SuperPriv, TriggerPriv, UpdatePriv, UsagePriv,
};
use tidb_parser::auth::RoleIdentity;
use tidb_types::stringutil::{compile_pattern_binary, do_match_binary};

/// Unicode case-insensitive comparison matching Go `strings.EqualFold` (with ASCII fast path).
// glue: Go strings.EqualFold
#[inline]
fn eq_fold(a: &str, b: &str) -> bool {
    if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(b)
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

/// Host specificity rank matching Go `compareHost` (`pkg/privilege/privileges/cache.go`):
/// exact non-empty host < `prefix%` (longer first) < `""` < `"%"`.
// go: compareHost L2 (adds deterministic lexicographical tie-break between equal-length `prefix%` hosts for Rust's strict weak ordering requirement)
fn compare_host(x: &str, y: &str) -> Ordering {
    let rank = |h: &str| -> u8 {
        if h == "%" {
            3
        } else if h.is_empty() {
            2
        } else if h.ends_with('%') {
            1
        } else {
            0
        }
    };
    let rx = rank(x);
    let ry = rank(y);
    if rx != ry {
        return rx.cmp(&ry);
    }
    match rx {
        // Longer wildcard prefix sorts first (e.g. `192.168.199.%` before `192.168.%`),
        // with deterministic lexicographical tie-break for total ordering.
        1 => y.len().cmp(&x.len()).then_with(|| x.cmp(y)),
        _ => x.cmp(y),
    }
}

// go: compareBaseRecord L1
fn compare_base_record(x: &BaseRecord, y: &BaseRecord) -> Ordering {
    compare_host(&x.host, &y.host).then_with(|| x.user.cmp(&y.user))
}

// go: compareUserRecord L1
fn compare_user_record(x: &UserRecord, y: &UserRecord) -> Ordering {
    compare_base_record(&x.base, &y.base)
}

// go: compareDBRecord L1
fn compare_db_record(x: &DbRecord, y: &DbRecord) -> Ordering {
    compare_base_record(&x.base, &y.base).then_with(|| x.db.cmp(&y.db))
}

// go: compareTablesPrivRecord L1
fn compare_tables_priv_record(x: &TablesPrivRecord, y: &TablesPrivRecord) -> Ordering {
    compare_base_record(&x.base, &y.base)
        .then_with(|| x.db.cmp(&y.db))
        .then_with(|| x.table_name.cmp(&y.table_name))
}

// go: compareColumnsPrivRecord L1
fn compare_columns_priv_record(x: &ColumnsPrivRecord, y: &ColumnsPrivRecord) -> Ordering {
    compare_base_record(&x.base, &y.base)
        .then_with(|| x.db.cmp(&y.db))
        .then_with(|| x.table_name.cmp(&y.table_name))
        .then_with(|| x.column_name.cmp(&y.column_name))
}

// go: compareDynamicPrivRecord L1
fn compare_dynamic_priv_record(x: &DynamicPrivRecord, y: &DynamicPrivRecord) -> Ordering {
    compare_base_record(&x.base, &y.base)
}

// go: compareDefaultRoleRecord L1
fn compare_default_role_record(x: &DefaultRoleRecord, y: &DefaultRoleRecord) -> Ordering {
    compare_base_record(&x.base, &y.base)
}

const globalDBVisible: u64 = CreatePriv.0
    | SelectPriv.0
    | InsertPriv.0
    | UpdatePriv.0
    | DeletePriv.0
    | ShowDBPriv.0
    | DropPriv.0
    | AlterPriv.0
    | IndexPriv.0
    | CreateViewPriv.0
    | ShowViewPriv.0
    | GrantPriv.0
    | TriggerPriv.0
    | ReferencesPriv.0
    | ExecutePriv.0;

/// IPv4 network (`<host_ip>/<netmask>`) parsed by [`parse_host_ip_net`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostIpNet {
    pub ip: Ipv4Addr,
    pub mask: Ipv4Addr,
}

impl HostIpNet {
    // glue: net.IPNet.Contains for IPv4
    fn contains(&self, candidate: Ipv4Addr) -> bool {
        let ip = u32::from(self.ip);
        let mask = u32::from(self.mask);
        let cand = u32::from(candidate);
        (cand & mask) == (ip & mask)
    }
}

/// Parses MySQL `<ipv4>/<netmask>` host specifications (e.g. `127.0.0.0/255.255.255.0`).
// go: parseHostIPNet L1
fn parse_host_ip_net(s: &str) -> Option<HostIpNet> {
    let (host_str, mask_str) = s.split_once('/')?;
    let host_ip: Ipv4Addr = host_str.parse().ok()?;
    let mask_ip: Ipv4Addr = mask_str.parse().ok()?;
    // Ensure <host_ip> & <netmask> == <host_ip> (MySQL account-names rules).
    if (u32::from(host_ip) & u32::from(mask_ip)) != u32::from(host_ip) {
        return None;
    }
    Some(HostIpNet {
        ip: host_ip,
        mask: mask_ip,
    })
}

/// patternMatch matches `%` and `_` in binary mode, as Go's `stringutil.DoMatchBinary`.
// go: patternMatch L1
fn pattern_match(s: &str, pat_chars: &[u8], pat_types: &[u8]) -> bool {
    do_match_binary(s.as_bytes(), pat_chars, pat_types)
}

/// baseRecord stores `Host` and `User` plus compiled pattern / netmask matchers.
#[derive(Clone, Debug)]
pub struct BaseRecord {
    pub host: String,
    pub user: String,
    pub pat_chars: Vec<u8>,
    pub pat_types: Vec<u8>,
    pub host_ip_net: Option<HostIpNet>,
}

impl BaseRecord {
    /// Builds a `BaseRecord` compiling the host pattern and optional IPv4 netmask.
    // go-part: baseRecord.assignUserOrHost L1
    pub fn new(user: impl Into<String>, host: impl Into<String>) -> Self {
        let user = user.into();
        let host = host.into();
        let (pat_chars, pat_types) = compile_pattern_binary(host.as_bytes(), b'\\');
        let host_ip_net = parse_host_ip_net(&host);
        Self {
            host,
            user,
            pat_chars,
            pat_types,
            host_ip_net,
        }
    }

    // go: baseRecord.hostMatch L1
    pub fn host_match(&self, s: &str) -> bool {
        let Some(net) = self.host_ip_net else {
            if self.host == "localhost"
                && let Ok(ip) = s.parse::<IpAddr>()
                && ip.is_loopback()
            {
                return true;
            }
            return false;
        };
        let Ok(ip) = s.parse::<Ipv4Addr>() else {
            return false;
        };
        net.contains(ip)
    }

    // go: baseRecord.match L2 (also normalizes IPv4-mapped IPv6 `::ffff:a.b.c.d` to IPv4)
    pub fn matches(&self, user: &str, host: &str) -> bool {
        if self.user != user {
            return false;
        }
        let normalized;
        let host = if let Ok(IpAddr::V6(v6)) = host.parse::<IpAddr>()
            && let Some(v4) = v6.to_ipv4_mapped()
        {
            normalized = v4.to_string();
            normalized.as_str()
        } else {
            host
        };
        pattern_match(host, &self.pat_chars, &self.pat_types) || self.host_match(host)
    }

    // go: baseRecord.fullyMatch L1
    pub fn fully_match(&self, user: &str, host: &str) -> bool {
        self.user == user && self.host == host
    }
}

/// UserRecord represents a row in `mysql.user`.
#[derive(Clone, Debug)]
pub struct UserRecord {
    pub base: BaseRecord,
    pub privileges: PrivilegeType,
    pub auth_string: String,
    pub auth_plugin: String,
    pub account_locked: bool,
    pub password_expired: bool,
    /// `mysql.user.password_lifetime`: `None` (SQL `NULL`) inherits global
    /// `default_password_lifetime`; `Some(0)` means `PASSWORD EXPIRE NEVER`; `Some(n)` (`n > 0`)
    /// configures per-account expiration after `n` days.
    pub password_lifetime: Option<u16>,
    pub has_failed_login_policy: bool,
}

impl UserRecord {
    // go: NewUserRecord L1
    pub fn new(
        host: impl Into<String>,
        user: impl Into<String>,
        privileges: PrivilegeType,
    ) -> Self {
        Self {
            base: BaseRecord::new(user, host),
            privileges,
            auth_string: String::new(),
            auth_plugin: "mysql_native_password".to_owned(),
            account_locked: false,
            password_expired: false,
            password_lifetime: None,
            has_failed_login_policy: false,
        }
    }

    /// Sets `mysql.user.authentication_string` (used for password verification on login).
    // glue: builder helper for authentication_string
    pub fn with_auth_string(mut self, auth_string: impl Into<String>) -> Self {
        self.auth_string = auth_string.into();
        self
    }

    /// Sets `mysql.user.plugin` (empty plugin normalizes to `mysql_native_password`, matching Go).
    // glue: builder helper for plugin
    pub fn with_auth_plugin(mut self, plugin: impl Into<String>) -> Self {
        let p = plugin.into();
        self.auth_plugin = if p.is_empty() {
            "mysql_native_password".to_owned()
        } else {
            p
        };
        self
    }

    /// Sets `mysql.user.Account_locked` (roles and locked accounts cannot log in).
    // glue: builder helper for account_locked
    pub fn with_account_locked(mut self, locked: bool) -> Self {
        self.account_locked = locked;
        self
    }

    /// Sets `mysql.user.password_expired` (`Y` = expired).
    // glue: builder helper for password_expired
    pub fn with_password_expired(mut self, expired: bool) -> Self {
        self.password_expired = expired;
        self
    }

    /// Sets `mysql.user.password_lifetime` (`None` = SQL `NULL`, `Some(0)` = NEVER, `Some(n)` = days).
    // glue: builder helper for password_lifetime
    pub fn with_password_lifetime(mut self, lifetime: Option<u16>) -> Self {
        self.password_lifetime = lifetime;
        self
    }

    /// Sets whether `mysql.user.User_attributes` configures failed-login locking or auto-lock.
    // glue: builder helper for failed-login policy
    pub fn with_failed_login_policy(mut self, has_policy: bool) -> Self {
        self.has_failed_login_policy = has_policy;
        self
    }
}

/// GlobalPrivRecord represents a row in `mysql.global_priv` (TLS/SSL/X509/SAN requirements).
#[derive(Clone, Debug)]
pub struct GlobalPrivRecord {
    pub base: BaseRecord,
    pub require_ssl: bool,
    pub broken: bool,
}

impl GlobalPrivRecord {
    // glue: constructor
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        require_ssl: bool,
        broken: bool,
    ) -> Self {
        Self {
            base: BaseRecord::new(user, host),
            require_ssl,
            broken,
        }
    }
}

/// DbRecord represents a row in `mysql.db`.
#[derive(Clone, Debug)]
pub struct DbRecord {
    pub base: BaseRecord,
    pub db: String,
    pub privileges: PrivilegeType,
    pub db_pat_chars: Vec<u8>,
    pub db_pat_types: Vec<u8>,
}

impl DbRecord {
    // glue: constructor compiling upper-case DB pattern as decodeDBTableRow
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        db: impl Into<String>,
        privileges: PrivilegeType,
    ) -> Self {
        let db = db.into();
        let upper = db.to_uppercase();
        let (db_pat_chars, db_pat_types) = compile_pattern_binary(upper.as_bytes(), b'\\');
        Self {
            base: BaseRecord::new(user, host),
            db,
            privileges,
            db_pat_chars,
            db_pat_types,
        }
    }

    // go: dbRecord.match L1
    pub fn matches(&self, user: &str, host: &str, db: &str) -> bool {
        self.base.matches(user, host)
            && pattern_match(&db.to_uppercase(), &self.db_pat_chars, &self.db_pat_types)
    }
}

/// TablesPrivRecord represents a row in `mysql.tables_priv`.
#[derive(Clone, Debug)]
pub struct TablesPrivRecord {
    pub base: BaseRecord,
    pub db: String,
    pub table_name: String,
    pub table_priv: PrivilegeType,
    pub column_priv: PrivilegeType,
}

impl TablesPrivRecord {
    // glue: constructor
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        db: impl Into<String>,
        table_name: impl Into<String>,
        table_priv: PrivilegeType,
    ) -> Self {
        Self {
            base: BaseRecord::new(user, host),
            db: db.into(),
            table_name: table_name.into(),
            table_priv,
            column_priv: PrivilegeType(0),
        }
    }

    /// Sets `mysql.tables_priv.Column_priv` (checked by `db_is_visible`).
    // glue: builder helper for column_priv
    pub fn with_column_priv(mut self, column_priv: PrivilegeType) -> Self {
        self.column_priv = column_priv;
        self
    }

    // go: tablesPrivRecord.match L1
    pub fn matches(&self, user: &str, host: &str, db: &str, table: &str) -> bool {
        self.base.matches(user, host) && eq_fold(&self.db, db) && eq_fold(&self.table_name, table)
    }
}

/// ColumnsPrivRecord represents a row in `mysql.columns_priv`.
#[derive(Clone, Debug)]
pub struct ColumnsPrivRecord {
    pub base: BaseRecord,
    pub db: String,
    pub table_name: String,
    pub column_name: String,
    pub column_priv: PrivilegeType,
}

impl ColumnsPrivRecord {
    // glue: constructor
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        db: impl Into<String>,
        table_name: impl Into<String>,
        column_name: impl Into<String>,
        column_priv: PrivilegeType,
    ) -> Self {
        Self {
            base: BaseRecord::new(user, host),
            db: db.into(),
            table_name: table_name.into(),
            column_name: column_name.into(),
            column_priv,
        }
    }

    /// `SELECT COUNT(*) ...` and table-read access checks (`column = "*"`, R29) require a
    /// column-level `SELECT` privilege on any column of the table. Keeping the
    /// `col == "*" && (self.column_priv & SelectPriv > 0)` predicate inside per-record matching
    /// ensures a user with both `UPDATE(a)` and `SELECT(b)` still matches `SELECT(b)` when
    /// `MatchColumns` scans the user's column records (specs/D04.1-privilege/spec.md §5.2 item 1).
    // go: columnsPrivRecord.match L1
    pub fn matches(&self, user: &str, host: &str, db: &str, table: &str, col: &str) -> bool {
        self.base.matches(user, host)
            && eq_fold(&self.db, db)
            && eq_fold(&self.table_name, table)
            && (eq_fold(&self.column_name, col)
                || (col == "*" && (self.column_priv.0 & SelectPriv.0) > 0))
    }
}

/// DynamicPrivRecord represents a row in `mysql.global_grants`.
#[derive(Clone, Debug)]
pub struct DynamicPrivRecord {
    pub base: BaseRecord,
    pub privilege_name: String,
    pub grant_option: bool,
}

impl DynamicPrivRecord {
    // glue: constructor
    pub fn new(
        user: impl Into<String>,
        host: impl Into<String>,
        privilege_name: impl Into<String>,
        grant_option: bool,
    ) -> Self {
        Self {
            base: BaseRecord::new(user, host),
            privilege_name: privilege_name.into().to_uppercase(),
            grant_option,
        }
    }
}

/// DefaultRoleRecord represents a row in `mysql.default_roles`.
#[derive(Clone, Debug)]
pub struct DefaultRoleRecord {
    pub base: BaseRecord,
    pub default_role_user: String,
    pub default_role_host: String,
}

/// Canonical map key for a role `(username, hostname)` (Go uses `auth.RoleIdentity` by value).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct RoleKey {
    username: String,
    hostname: String,
}

impl From<&RoleIdentity> for RoleKey {
    // glue: role key conversion
    fn from(r: &RoleIdentity) -> Self {
        Self {
            username: r.username.clone(),
            hostname: r.hostname.clone(),
        }
    }
}

/// roleGraphEdgesTable caches outgoing role grants for a user/role.
#[derive(Clone, Debug, Default)]
struct RoleGraphEdgesTable {
    role_list: BTreeMap<RoleKey, RoleIdentity>,
}

impl RoleGraphEdgesTable {
    // go: roleGraphEdgesTable.Find L1
    fn find(&self, user: &str, host: &str) -> bool {
        let mut key = RoleKey {
            username: user.to_owned(),
            hostname: host.to_owned(),
        };
        if self.role_list.contains_key(&key) {
            return true;
        }
        if key.hostname.is_empty() {
            key.hostname = "%".to_owned();
            return self.role_list.contains_key(&key);
        }
        false
    }
}

/// MySQLPrivilege is the in-memory cache of MySQL grant tables.
#[derive(Clone, Debug, Default)]
pub struct MySQLPrivilege {
    user: HashMap<String, Vec<UserRecord>>,
    global_priv: HashMap<String, Vec<GlobalPrivRecord>>,
    db: HashMap<String, Vec<DbRecord>>,
    tables_priv: HashMap<String, Vec<TablesPrivRecord>>,
    columns_priv: HashMap<String, Vec<ColumnsPrivRecord>>,
    dynamic_priv: HashMap<String, Vec<DynamicPrivRecord>>,
    default_roles: HashMap<String, Vec<DefaultRoleRecord>>,
    role_graph: HashMap<RoleKey, RoleGraphEdgesTable>,
    /// Cluster `@@global.default_password_lifetime` loaded from `mysql.global_variables` (0 = never).
    default_password_lifetime: u64,
}

impl MySQLPrivilege {
    /// Creates an empty privilege cache.
    // glue: constructor
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the cluster `default_password_lifetime` (days; `0` disables default expiration).
    // glue: global variable helper
    pub fn set_default_password_lifetime(&mut self, days: u64) {
        self.default_password_lifetime = days;
    }

    /// Returns the cluster `default_password_lifetime` (days; `0` = never expires by default).
    // glue: global variable helper
    pub fn default_password_lifetime(&self) -> u64 {
        self.default_password_lifetime
    }

    /// Appends a `mysql.user` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_user(&mut self, record: UserRecord) {
        self.user
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.user` record and maintains Go's host-specificity sort order.
    // glue: builder helper for tests and loaders
    pub fn add_user(&mut self, record: UserRecord) {
        let recs = self.user.entry(record.base.user.clone()).or_default();
        recs.push(record);
        recs.sort_by(compare_user_record);
    }

    /// Appends a `mysql.db` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_db(&mut self, record: DbRecord) {
        self.db
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.db` record and maintains Go's host-specificity sort order.
    // glue: builder helper for tests and loaders
    pub fn add_db(&mut self, record: DbRecord) {
        let recs = self.db.entry(record.base.user.clone()).or_default();
        recs.push(record);
        recs.sort_by(compare_db_record);
    }

    /// Appends a `mysql.tables_priv` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_table_priv(&mut self, record: TablesPrivRecord) {
        self.tables_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.tables_priv` record and maintains Go's host-specificity sort order.
    // glue: builder helper for tests and loaders
    pub fn add_table_priv(&mut self, record: TablesPrivRecord) {
        let recs = self
            .tables_priv
            .entry(record.base.user.clone())
            .or_default();
        recs.push(record);
        recs.sort_by(compare_tables_priv_record);
    }

    /// Appends a `mysql.columns_priv` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_column_priv(&mut self, record: ColumnsPrivRecord) {
        self.columns_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.columns_priv` record and maintains Go's host-specificity sort order.
    // glue: builder helper for tests and loaders
    pub fn add_column_priv(&mut self, record: ColumnsPrivRecord) {
        let recs = self
            .columns_priv
            .entry(record.base.user.clone())
            .or_default();
        recs.push(record);
        recs.sort_by(compare_columns_priv_record);
    }

    /// Appends a `mysql.global_grants` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_dynamic_priv(&mut self, record: DynamicPrivRecord) {
        self.dynamic_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.global_grants` dynamic privilege record and maintains Go's sort order.
    // glue: builder helper for tests and loaders
    pub fn add_dynamic_priv(&mut self, record: DynamicPrivRecord) {
        let recs = self
            .dynamic_priv
            .entry(record.base.user.clone())
            .or_default();
        recs.push(record);
        recs.sort_by(compare_dynamic_priv_record);
    }

    /// Appends a `mysql.default_roles` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_default_role(
        &mut self,
        user: impl Into<String>,
        host: impl Into<String>,
        role_user: impl Into<String>,
        role_host: impl Into<String>,
    ) {
        let rec = DefaultRoleRecord {
            base: BaseRecord::new(user, host),
            default_role_user: role_user.into(),
            default_role_host: role_host.into(),
        };
        self.default_roles
            .entry(rec.base.user.clone())
            .or_default()
            .push(rec);
    }

    /// Registers a `mysql.default_roles` record and maintains Go's sort order.
    // glue: builder helper for tests and loaders
    pub fn add_default_role(
        &mut self,
        user: impl Into<String>,
        host: impl Into<String>,
        role_user: impl Into<String>,
        role_host: impl Into<String>,
    ) {
        let rec = DefaultRoleRecord {
            base: BaseRecord::new(user, host),
            default_role_user: role_user.into(),
            default_role_host: role_host.into(),
        };
        let recs = self.default_roles.entry(rec.base.user.clone()).or_default();
        recs.push(rec);
        recs.sort_by(compare_default_role_record);
    }

    /// Appends a `mysql.global_priv` record without sorting; caller must invoke [`Self::sort_all`].
    // glue: bulk-load helper
    pub fn push_global_priv(&mut self, record: GlobalPrivRecord) {
        self.global_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.global_priv` record and maintains Go's host-specificity sort order.
    // glue: builder helper for tests and loaders
    pub fn add_global_priv(&mut self, record: GlobalPrivRecord) {
        let recs = self
            .global_priv
            .entry(record.base.user.clone())
            .or_default();
        recs.push(record);
        recs.sort_by(|x, y| compare_base_record(&x.base, &y.base));
    }

    /// Sorts every privilege bucket using Go's host-specificity order (`SortUserTable`,
    /// `LoadDBTable`, `LoadTablesPrivTable`, `LoadColumnsPrivTable`, `LoadGlobalGrantsTable`).
    // go: MySQLPrivilege.SortUserTable L1
    pub fn sort_all(&mut self) {
        for recs in self.user.values_mut() {
            recs.sort_by(compare_user_record);
        }
        for recs in self.global_priv.values_mut() {
            recs.sort_by(|x, y| compare_base_record(&x.base, &y.base));
        }
        for recs in self.db.values_mut() {
            recs.sort_by(compare_db_record);
        }
        for recs in self.tables_priv.values_mut() {
            recs.sort_by(compare_tables_priv_record);
        }
        for recs in self.columns_priv.values_mut() {
            recs.sort_by(compare_columns_priv_record);
        }
        for recs in self.dynamic_priv.values_mut() {
            recs.sort_by(compare_dynamic_priv_record);
        }
        for recs in self.default_roles.values_mut() {
            recs.sort_by(compare_default_role_record);
        }
    }

    /// Registers a `mysql.role_edges` grant: `from_role` is granted to `(to_user, to_host)`.
    // glue: builder helper for role graph
    pub fn add_role_edge(
        &mut self,
        from_user: impl Into<String>,
        from_host: impl Into<String>,
        to_user: impl Into<String>,
        to_host: impl Into<String>,
    ) {
        let to_key = RoleKey {
            username: to_user.into(),
            hostname: to_host.into(),
        };
        let from_role = RoleIdentity {
            username: from_user.into(),
            hostname: from_host.into(),
        };
        self.role_graph
            .entry(to_key)
            .or_default()
            .role_list
            .insert(RoleKey::from(&from_role), from_role);
    }

    /// Removes all `tables_priv` / `columns_priv` / `db` / global privileges for `user`.
    // glue: test helper to simulate REVOKE in live Handle updates
    #[cfg(test)]
    pub fn revoke_user_all_privs(&mut self, user: &str) {
        if let Some(recs) = self.user.get_mut(user) {
            for r in recs {
                r.privileges = PrivilegeType(0);
            }
        }
        self.db.remove(user);
        self.tables_priv.remove(user);
        self.columns_priv.remove(user);
        self.dynamic_priv.remove(user);
    }

    // go: MySQLPrivilege.matchUser L1
    pub fn match_user(&self, user: &str, host: &str) -> Option<&UserRecord> {
        self.user
            .get(user)?
            .iter()
            .find(|r| r.base.matches(user, host))
    }

    // go: MySQLPrivilege.matchGlobalPriv L1
    pub fn match_global_priv(&self, user: &str, host: &str) -> Option<&GlobalPrivRecord> {
        self.global_priv
            .get(user)?
            .iter()
            .find(|r| r.base.matches(user, host))
    }

    // go: MySQLPrivilege.matchDB L1
    pub fn match_db(&self, user: &str, host: &str, db: &str) -> Option<&DbRecord> {
        self.db
            .get(user)?
            .iter()
            .find(|r| r.matches(user, host, db))
    }

    // go: MySQLPrivilege.matchTables L1
    pub fn match_tables(
        &self,
        user: &str,
        host: &str,
        db: &str,
        table: &str,
    ) -> Option<&TablesPrivRecord> {
        self.tables_priv
            .get(user)?
            .iter()
            .find(|r| r.matches(user, host, db, table))
    }

    // go: MySQLPrivilege.MatchColumns L1
    pub fn match_columns(
        &self,
        user: &str,
        host: &str,
        db: &str,
        table: &str,
        column: &str,
    ) -> Option<&ColumnsPrivRecord> {
        self.columns_priv
            .get(user)?
            .iter()
            .find(|r| r.matches(user, host, db, table, column))
    }

    // go: MySQLPrivilege.FindRole L1
    pub fn find_role(&self, user: &str, host: &str, role: &RoleIdentity) -> bool {
        let Some(rec) = self.match_user(user, host) else {
            return false;
        };
        if self.match_user(&role.username, &role.hostname).is_none() {
            return false;
        }
        let key = RoleKey {
            username: rec.base.user.clone(),
            hostname: rec.base.host.clone(),
        };
        self.role_graph
            .get(&key)
            .is_some_and(|edges| edges.find(&role.username, &role.hostname))
    }

    // go: MySQLPrivilege.FindAllRole L1 [M-MAPORDER]
    pub fn find_all_role(&self, active_roles: &[RoleIdentity]) -> Vec<RoleIdentity> {
        let mut queue: VecDeque<RoleIdentity> = active_roles.iter().cloned().collect();
        let mut visited: HashSet<RoleKey> = HashSet::new();
        let mut ret: Vec<RoleIdentity> = Vec::new();
        while let Some(role) = queue.pop_front() {
            let key = RoleKey::from(&role);
            if visited.insert(key.clone()) {
                if let Some(edges) = self.role_graph.get(&key) {
                    for (next_key, next_role) in &edges.role_list {
                        if !visited.contains(next_key) {
                            queue.push_back(next_role.clone());
                        }
                    }
                }
                ret.push(role);
            }
        }
        ret
    }

    // go: MySQLPrivilege.FindAllUserEffectiveRoles L1
    pub fn find_all_user_effective_roles(
        &self,
        user: &str,
        host: &str,
        active_roles: &[RoleIdentity],
    ) -> Vec<RoleIdentity> {
        let mut granted = Vec::with_capacity(active_roles.len());
        for role in active_roles {
            if self.find_role(user, host, role) {
                granted.push(role.clone());
            }
        }
        self.find_all_role(&granted)
    }

    // go: MySQLPrivilege.getDefaultRoles L1
    pub fn get_default_roles(&self, user: &str, host: &str) -> Vec<RoleIdentity> {
        let Some(records) = self.default_roles.get(user) else {
            return Vec::new();
        };
        let mut ret = Vec::new();
        for rec in records {
            if rec.base.matches(user, host) {
                ret.push(RoleIdentity {
                    username: rec.default_role_user.clone(),
                    hostname: rec.default_role_host.clone(),
                });
            }
        }
        ret
    }

    // go: MySQLPrivilege.HasExplicitlyGrantedDynamicPrivilege L1
    pub fn has_explicitly_granted_dynamic_privilege(
        &self,
        active_roles: &[RoleIdentity],
        user: &str,
        host: &str,
        priv_name: &str,
        with_grant: bool,
    ) -> bool {
        let priv_name = priv_name.to_uppercase();
        let mut role_list = self.find_all_user_effective_roles(user, host, active_roles);
        role_list.push(RoleIdentity {
            username: user.to_owned(),
            hostname: host.to_owned(),
        });
        for r in &role_list {
            if let Some(records) = self.dynamic_priv.get(&r.username) {
                for rec in records {
                    if rec.base.matches(&r.username, &r.hostname) {
                        if with_grant && !rec.grant_option {
                            continue;
                        }
                        if rec.privilege_name == priv_name {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    // go: MySQLPrivilege.RequestDynamicVerification L2 (SEM is not ported yet)
    pub fn request_dynamic_verification(
        &self,
        active_roles: &[RoleIdentity],
        user: &str,
        host: &str,
        priv_name: &str,
        with_grant: bool,
    ) -> bool {
        let priv_name = priv_name.to_uppercase();
        if self.has_explicitly_granted_dynamic_privilege(
            active_roles,
            user,
            host,
            &priv_name,
            with_grant,
        ) {
            return true;
        }
        if with_grant && !self.request_verification(active_roles, user, host, "", "", "", GrantPriv)
        {
            return false;
        }
        self.request_verification(active_roles, user, host, "", "", "", SuperPriv)
    }

    /// RequestVerification checks whether the user has sufficient privileges to do the operation.
    /// `column == "*"` matches ANY column with `SelectPriv` in the table; `column == ""` requires
    /// global, database, or table-level privilege.
    // go: MySQLPrivilege.RequestVerification L1
    #[allow(clippy::too_many_arguments)]
    pub fn request_verification(
        &self,
        active_roles: &[RoleIdentity],
        user: &str,
        host: &str,
        db: &str,
        table: &str,
        column: &str,
        priv_: PrivilegeType,
    ) -> bool {
        if priv_ == UsagePriv {
            return true;
        }

        let mut role_list = self.find_all_user_effective_roles(user, host, active_roles);
        role_list.push(RoleIdentity {
            username: user.to_owned(),
            hostname: host.to_owned(),
        });

        let mut user_priv: u64 = 0;
        for r in &role_list {
            if let Some(rec) = self.match_user(&r.username, &r.hostname) {
                user_priv |= rec.privileges.0;
            }
        }
        if (user_priv & priv_.0) > 0 {
            return true;
        }

        let mut db_priv: u64 = 0;
        for r in &role_list {
            if let Some(rec) = self.match_db(&r.username, &r.hostname, db) {
                db_priv |= rec.privileges.0;
            }
        }
        if (db_priv & priv_.0) > 0 {
            return true;
        }

        let mut table_priv: u64 = 0;
        let mut column_priv: u64 = 0;
        for r in &role_list {
            if let Some(rec) = self.match_tables(&r.username, &r.hostname, db, table) {
                table_priv |= rec.table_priv.0;
                if !column.is_empty() {
                    column_priv |= rec.table_priv.0;
                }
            }
        }
        if (table_priv & priv_.0) > 0 {
            return true;
        }

        for r in &role_list {
            if let Some(rec) = self.match_columns(&r.username, &r.hostname, db, table, column) {
                column_priv |= rec.column_priv.0;
            }
        }
        if (column_priv & priv_.0) > 0 {
            return true;
        }

        priv_.0 == 0
    }

    // go: MySQLPrivilege.DBIsVisible L1
    pub fn db_is_visible(&self, user: &str, host: &str, db: &str) -> bool {
        if let Some(rec) = self.match_user(user, host) {
            if (rec.privileges.0 & globalDBVisible) > 0 {
                return true;
            }
            // For metrics_schema, `PROCESS` also grants visibility.
            if (rec.privileges.0 & ProcessPriv.0) > 0 && eq_fold(db, "metrics_schema") {
                return true;
            }
        }
        // INFORMATION_SCHEMA is visible to all users.
        if eq_fold(db, "INFORMATION_SCHEMA") {
            return true;
        }
        if let Some(rec) = self.match_db(user, host, db)
            && rec.privileges.0 > 0
        {
            return true;
        }
        if let Some(records) = self.tables_priv.get(user) {
            for rec in records {
                if rec.base.matches(user, host)
                    && eq_fold(&rec.db, db)
                    && (rec.table_priv.0 != 0 || rec.column_priv.0 != 0)
                {
                    return true;
                }
            }
        }
        if let Some(records) = self.columns_priv.get(user) {
            for rec in records {
                if rec.base.matches(user, host) && eq_fold(&rec.db, db) && rec.column_priv.0 != 0 {
                    return true;
                }
            }
        }
        false
    }
}

/// Handle wraps a live, lock-free atomically replaceable [`MySQLPrivilege`] snapshot (using
/// `ArcSwap`, matching Go's `atomic.Pointer[MySQLPrivilege]`) so `GRANT` / `REVOKE` updates take
/// effect immediately on existing sessions and cached plans.
#[derive(Debug, Default)]
pub struct Handle {
    priv_: ArcSwap<MySQLPrivilege>,
}

impl Handle {
    // go: NewHandle L2 (in-memory snapshot; cluster mysql.* loader wired in Part 5/5)
    pub fn new(mut initial: MySQLPrivilege) -> Self {
        initial.sort_all();
        Self {
            priv_: ArcSwap::from_pointee(initial),
        }
    }

    // go: Handle.Get L1
    pub fn get(&self) -> Arc<MySQLPrivilege> {
        self.priv_.load_full()
    }

    /// Atomically installs a new [`MySQLPrivilege`] snapshot (e.g. after `GRANT` / `REVOKE`).
    // glue: live snapshot update
    pub fn replace(&self, mut next: MySQLPrivilege) {
        next.sort_all();
        self.priv_.store(Arc::new(next));
    }
}
