//! go-file: pkg/privilege/privileges/cache.go
//!
//! Faithful port of Go TiDB v8.5.8 `MySQLPrivilege` in-memory grant cache and verification
//! engine (`RequestVerification`, `RequestDynamicVerification`, `MatchColumns`, role graph
//! traversal, and reloadable `Handle`).

#![allow(non_upper_case_globals)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use parking_lot::RwLock;
use tidb_base::mysql::{
    AlterPriv, CreatePriv, CreateViewPriv, DeletePriv, DropPriv, ExecutePriv, GrantPriv, IndexPriv,
    InsertPriv, PrivilegeType, ReferencesPriv, SelectPriv, ShowDBPriv, ShowViewPriv, SuperPriv,
    TriggerPriv, UpdatePriv, UsagePriv,
};
use tidb_parser::auth::RoleIdentity;
use tidb_types::stringutil::{compile_pattern_binary, do_match_binary};

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

    // go: baseRecord.match L1
    pub fn matches(&self, user: &str, host: &str) -> bool {
        self.user == user
            && (pattern_match(host, &self.pat_chars, &self.pat_types) || self.host_match(host))
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
    pub account_locked: bool,
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
            account_locked: false,
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

    // go: tablesPrivRecord.match L1
    pub fn matches(&self, user: &str, host: &str, db: &str, table: &str) -> bool {
        self.base.matches(user, host)
            && self.db.eq_ignore_ascii_case(db)
            && self.table_name.eq_ignore_ascii_case(table)
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
            && self.db.eq_ignore_ascii_case(db)
            && self.table_name.eq_ignore_ascii_case(table)
            && (self.column_name.eq_ignore_ascii_case(col)
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
        self.role_list.contains_key(&RoleKey {
            username: user.to_owned(),
            hostname: host.to_owned(),
        })
    }
}

/// MySQLPrivilege is the in-memory cache of MySQL grant tables.
#[derive(Clone, Debug, Default)]
pub struct MySQLPrivilege {
    user: HashMap<String, Vec<UserRecord>>,
    db: HashMap<String, Vec<DbRecord>>,
    tables_priv: HashMap<String, Vec<TablesPrivRecord>>,
    columns_priv: HashMap<String, Vec<ColumnsPrivRecord>>,
    dynamic_priv: HashMap<String, Vec<DynamicPrivRecord>>,
    default_roles: HashMap<String, Vec<DefaultRoleRecord>>,
    role_graph: HashMap<RoleKey, RoleGraphEdgesTable>,
}

impl MySQLPrivilege {
    /// Creates an empty privilege cache.
    // glue: constructor
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `mysql.user` record.
    // glue: builder helper for tests and callers
    pub fn add_user(&mut self, record: UserRecord) {
        self.user
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.db` record.
    // glue: builder helper for tests and callers
    pub fn add_db(&mut self, record: DbRecord) {
        self.db
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.tables_priv` record.
    // glue: builder helper for tests and callers
    pub fn add_table_priv(&mut self, record: TablesPrivRecord) {
        self.tables_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.columns_priv` record.
    // glue: builder helper for tests and callers
    pub fn add_column_priv(&mut self, record: ColumnsPrivRecord) {
        self.columns_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.global_grants` dynamic privilege record.
    // glue: builder helper for tests and callers
    pub fn add_dynamic_priv(&mut self, record: DynamicPrivRecord) {
        self.dynamic_priv
            .entry(record.base.user.clone())
            .or_default()
            .push(record);
    }

    /// Registers a `mysql.default_roles` record.
    // glue: builder helper for tests and callers
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
        self.default_roles
            .entry(rec.base.user.clone())
            .or_default()
            .push(rec);
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
    // glue: helper to simulate REVOKE in live Handle updates
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
        if let Some(rec) = self.match_user(user, host)
            && (rec.privileges.0 & globalDBVisible) > 0
        {
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
                    && rec.db.eq_ignore_ascii_case(db)
                    && rec.table_priv.0 != 0
                {
                    return true;
                }
            }
        }
        if let Some(records) = self.columns_priv.get(user) {
            for rec in records {
                if rec.base.matches(user, host)
                    && rec.db.eq_ignore_ascii_case(db)
                    && rec.column_priv.0 != 0
                {
                    return true;
                }
            }
        }
        false
    }
}

/// Handle wraps a live, atomically replaceable [`MySQLPrivilege`] snapshot so `GRANT` / `REVOKE`
/// updates take effect immediately on existing sessions and cached plans.
#[derive(Debug, Default)]
pub struct Handle {
    priv_: RwLock<Arc<MySQLPrivilege>>,
}

impl Handle {
    // go: NewHandle L2 (in-memory snapshot; storage-backed UpdateAll comes in later D04.1 part)
    pub fn new(initial: MySQLPrivilege) -> Self {
        Self {
            priv_: RwLock::new(Arc::new(initial)),
        }
    }

    // go: Handle.Get L1
    pub fn get(&self) -> Arc<MySQLPrivilege> {
        Arc::clone(&self.priv_.read())
    }

    /// Atomically installs a new [`MySQLPrivilege`] snapshot (e.g. after `GRANT` / `REVOKE`).
    // glue: live snapshot update
    pub fn replace(&self, next: MySQLPrivilege) {
        *self.priv_.write() = Arc::new(next);
    }
}
