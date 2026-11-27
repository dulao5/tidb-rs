//! Cluster-backed `mysql.*` privilege table loading and live etcd `/tidb/privilege` reload
//! (D04.1 Part 5/5; Go: `pkg/privilege/privileges/cache.go`, `pkg/domain/domain.go`).
//!
//! Reads the seven MySQL privilege tables directly from TiKV at a single consistent TSO snapshot
//! using `tidb-tablecodec`:
//! - `mysql.user` (required)
//! - `mysql.db`
//! - `mysql.tables_priv`
//! - `mysql.columns_priv`
//! - `mysql.global_grants`
//! - `mysql.default_roles`
//! - `mysql.role_edges`
//!
//! Startup fails fast if the initial load fails (never silently falling back to open access).
//! Subsequent background reloads (triggered by etcd `/tidb/privilege` notifications, a 30s
//! periodic safety tick, or on-demand `reload_now()`) atomically swap the live
//! [`tidb_privilege::Handle`] snapshot via `ArcSwap`; transient load errors log a warning and
//! preserve the last known good snapshot (matching Go's `LoadPrivilegeLoop`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use etcd_client::{Client as EtcdClient, ConnectOptions, WatchOptions};
use poc_kv::{KeyRange, KvClient};
use tidb_base::gostd::time::utc;
use tidb_base::mysql::{PrivilegeType, new_priv_from_column, new_priv_from_set_enum};
use tidb_meta::keys as meta_keys;
use tidb_model::{ColumnInfo, IndexInfo, StatePublic, TableInfo};
use tidb_parser::types::FieldType;
use tidb_privilege::{
    ColumnsPrivRecord, DbRecord, DynamicPrivRecord, Handle, MySQLPrivilege, TablesPrivRecord,
    UserRecord,
};
use tidb_systable::session_io::MYSQL_DB_ID;
use tidb_tablecodec::tablecodec as tc;
use tidb_types::datum::{Datum, KindMysqlEnum, KindMysqlSet, KindString};
use tracing::{debug, info, warn};

use crate::{Error, Result, keys};

/// Periodic fallback reload interval in case an etcd notification is missed (matches Go's
/// `LoadPrivilegeLoop` periodic refresh).
const PRIVILEGE_REFRESH_TICK: Duration = Duration::from_secs(30);
/// Timeout of one etcd get/watch request.
const ETCD_OP_TIMEOUT: Duration = Duration::from_secs(2);

/// Live cluster privilege synchronization handle.
#[derive(Clone)]
pub struct PrivilegeSync {
    inner: Arc<Inner>,
}

struct Inner {
    kv: KvClient,
    handle: Arc<Handle>,
    /// Serializes reloads so an older snapshot can never overwrite a newer one.
    reload_lock: tokio::sync::Mutex<()>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl PrivilegeSync {
    /// Performs the initial load of `mysql.*` privilege tables from TiKV (failing fast on error)
    /// and starts the background etcd `/tidb/privilege` watcher + periodic refresh loop.
    pub async fn start(kv: KvClient, pd_endpoints: Vec<String>) -> Result<Self> {
        let initial = load_privileges_from_kv(&kv).await?;
        let handle = Arc::new(Handle::new(initial));
        let (stop, _) = tokio::sync::watch::channel(false);
        let inner = Arc::new(Inner {
            kv,
            handle,
            reload_lock: tokio::sync::Mutex::new(()),
            stop,
        });
        if !pd_endpoints.is_empty() {
            tokio::spawn(inner.clone().watch_loop(pd_endpoints));
        }
        tokio::spawn(inner.clone().periodic_refresh_loop());
        info!("cluster privilege cache loaded from mysql.*");
        Ok(Self { inner })
    }

    /// Creates a `PrivilegeSync` backed by an already-loaded `Handle` (for unit tests without
    /// TiKV/etcd).
    #[cfg(test)]
    pub fn from_handle_for_test(kv: KvClient, handle: Arc<Handle>) -> Self {
        let (stop, _) = tokio::sync::watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                kv,
                handle,
                reload_lock: tokio::sync::Mutex::new(()),
                stop,
            }),
        }
    }

    /// Returns the live lock-free [`Handle`] shared by all sessions.
    pub fn handle(&self) -> &Arc<Handle> {
        &self.inner.handle
    }

    /// Reloads all `mysql.*` privilege tables from TiKV at a fresh TSO and atomically swaps the
    /// [`Handle`] snapshot. On error, leaves the existing snapshot intact.
    pub async fn reload_now(&self) -> Result<()> {
        self.inner.reload().await
    }

    /// Stops background watch and periodic refresh tasks.
    pub fn shutdown(&self) {
        self.inner.stop.send_replace(true);
    }
}

impl Inner {
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    async fn reload(&self) -> Result<()> {
        let _guard = self.reload_lock.lock().await;
        let next = load_privileges_from_kv(&self.kv).await?;
        self.handle.replace(next);
        Ok(())
    }

    async fn periodic_refresh_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            tokio::select! {
                _ = tokio::time::sleep(PRIVILEGE_REFRESH_TICK) => {}
                _ = stop.changed() => return,
            }
            if self.stopped() {
                return;
            }
            if let Err(e) = self.reload().await {
                warn!(error = %e, "periodic privilege reload failed; keeping previous snapshot");
            }
        }
    }

    async fn watch_loop(self: Arc<Self>, pd_endpoints: Vec<String>) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            match self.watch_once(&pd_endpoints, &mut stop).await {
                Ok(()) => return,
                Err(e) => {
                    warn!(error = %e, "privilege etcd watch failed; retrying");
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                _ = stop.changed() => return,
            }
        }
    }

    async fn watch_once(
        &self,
        pd_endpoints: &[String],
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut etcd = EtcdClient::connect(
            pd_endpoints,
            Some(
                ConnectOptions::new()
                    .with_connect_timeout(Duration::from_secs(5))
                    .with_keep_alive(Duration::from_secs(10), Duration::from_secs(5)),
            ),
        )
        .await?;
        let resp = tokio::time::timeout(ETCD_OP_TIMEOUT, etcd.get(keys::PRIVILEGE_NOTIFY, None))
            .await
            .map_err(|_| Error::Timeout)??;
        let revision = resp.header().map_or(0, |h| h.revision());
        let (_watcher, mut stream) = tokio::time::timeout(
            ETCD_OP_TIMEOUT,
            etcd.watch(
                keys::PRIVILEGE_NOTIFY,
                Some(WatchOptions::new().with_start_revision(revision + 1)),
            ),
        )
        .await
        .map_err(|_| Error::Timeout)??;

        loop {
            let msg = tokio::select! {
                m = stream.message() => m?,
                _ = stop.changed() => return Ok(()),
            };
            let Some(msg) = msg else {
                return Err(Error::Other("privilege watch stream ended".to_owned()));
            };
            if !msg.events().is_empty() {
                debug!(
                    events = msg.events().len(),
                    "received /tidb/privilege notification; reloading privilege cache"
                );
                if let Err(e) = self.reload().await {
                    warn!(
                        error = %e,
                        "privilege reload on etcd notification failed; keeping previous snapshot"
                    );
                }
            }
            if msg.canceled() {
                return Err(Error::Other("privilege watch canceled".to_owned()));
            }
        }
    }
}

/// Loads all `mysql.*` privilege tables from TiKV at a single consistent TSO snapshot.
pub async fn load_privileges_from_kv(kv: &KvClient) -> Result<MySQLPrivilege> {
    let ts = kv.current_ts().await?;
    let tables = load_mysql_table_infos(kv, ts).await?;

    let mut mp = MySQLPrivilege::new();

    // `mysql.user` is mandatory (matching Go `MySQLPrivilege.LoadAll`).
    let user_tbl = tables
        .get("user")
        .ok_or_else(|| Error::Other("mysql.user table not found in cluster metadata".to_owned()))?;
    let user_rows = scan_table_records(kv, ts, user_tbl.id).await?;
    decode_mysql_user(user_tbl, &user_rows, &mut mp)?;

    // Remaining tables: load if present; fail the whole snapshot on decode/KV error.
    for (name, decoder) in [
        (
            "db",
            decode_mysql_db
                as fn(&TableInfo, &[(Vec<u8>, Vec<u8>)], &mut MySQLPrivilege) -> Result<()>,
        ),
        ("tables_priv", decode_mysql_tables_priv),
        ("columns_priv", decode_mysql_columns_priv),
        ("global_grants", decode_mysql_global_grants),
        ("default_roles", decode_mysql_default_roles),
        ("role_edges", decode_mysql_role_edges),
    ] {
        match tables.get(name) {
            Some(tbl) => {
                let rows = scan_table_records(kv, ts, tbl.id).await?;
                decoder(tbl, &rows, &mut mp)?;
            }
            None => {
                warn!(
                    table = name,
                    "optional mysql privilege table not found in metadata"
                );
            }
        }
    }

    mp.sort_all();
    Ok(mp)
}

/// Reads `TableInfo` entries under `mysql` DB metadata at `ts`.
async fn load_mysql_table_infos(kv: &KvClient, ts: u64) -> Result<HashMap<String, TableInfo>> {
    let prefix = meta_keys::hash_data_prefix(&meta_keys::db_key(MYSQL_DB_ID));
    let range = KeyRange::new(prefix.clone(), meta_keys::prefix_next(&prefix));
    let pairs = kv.snapshot(ts).scan_all(range).await?;
    let mut out = HashMap::new();
    for (k, v) in pairs {
        let Some(field) = meta_keys::decode_hash_field(&k, &prefix) else {
            continue;
        };
        if meta_keys::parse_table_key(&field).is_none() {
            continue;
        }
        let mut t = TableInfo::default();
        if t.unmarshal_json(&v).is_ok() && t.state == StatePublic {
            out.insert(t.name.l.clone(), t);
        }
    }
    Ok(out)
}

/// Scans all record keys `t<table_id>_r...` of `table_id` at snapshot `ts`.
async fn scan_table_records(
    kv: &KvClient,
    ts: u64,
    table_id: i64,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let start = tc::gen_table_record_prefix(table_id);
    let end = start.prefix_next();
    let pairs = kv
        .snapshot(ts)
        .scan_all(KeyRange::new(start.0, end.0))
        .await?;
    Ok(pairs)
}

/// Returns primary-key handle column IDs for `tbl` (int-handle PK or common-handle primary index).
fn handle_col_ids(tbl: &TableInfo) -> Vec<i64> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    if tbl.is_common_handle {
        if let Some(indices) = tbl.indices.as_deref()
            && let Some(pk) = indices.iter().find(|idx| idx.primary)
        {
            return primary_index_col_ids(pk, cols);
        }
    } else if tbl.pk_is_handle
        && let Some(pk_col) = cols
            .iter()
            .find(|c| tidb_base::mysql::has_pri_key_flag(c.field_type.get_flag()))
    {
        return vec![pk_col.id];
    }
    Vec::new()
}

fn primary_index_col_ids(pk: &IndexInfo, cols: &[Arc<ColumnInfo>]) -> Vec<i64> {
    pk.columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter_map(|ic| {
            usize::try_from(ic.offset)
                .ok()
                .and_then(|off| cols.get(off))
                .map(|c| c.id)
        })
        .collect()
}

/// Decodes a single TiKV `(key, value)` record into a `col_id -> Datum` map, combining row-value
/// columns with handle columns (for clustered / common-handle tables where binary-collated PK
/// columns live only in the key).
fn decode_record(tbl: &TableInfo, key: &[u8], val: &[u8]) -> Result<HashMap<i64, Datum>> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_types: HashMap<i64, &FieldType> = cols.iter().map(|c| (c.id, &c.field_type)).collect();
    let loc = utc();
    let row = tc::decode_row_to_datum_map(val, &col_types, Some(&loc))
        .map_err(|e| Error::Other(format!("decode row of mysql.{}: {e}", tbl.name.l)))?;
    let hcols = handle_col_ids(tbl);
    if hcols.is_empty() {
        return Ok(row);
    }
    let handle = tc::decode_row_key(key)
        .map_err(|e| Error::Other(format!("decode handle of mysql.{}: {e}", tbl.name.l)))?;
    let (merged, res) =
        tc::decode_handle_to_datum_map(Some(&handle), &hcols, &col_types, Some(&loc), Some(row));
    res.map_err(|e| Error::Other(format!("decode handle datums of mysql.{}: {e}", tbl.name.l)))?;
    Ok(merged)
}

#[allow(non_upper_case_globals)]
fn datum_string(d: &Datum) -> String {
    if d.is_null() {
        return String::new();
    }
    match d.kind() {
        KindMysqlEnum => String::from_utf8_lossy(&d.get_mysql_enum().name).into_owned(),
        KindMysqlSet => String::from_utf8_lossy(&d.get_mysql_set().name).into_owned(),
        KindString => String::from_utf8_lossy(d.get_string()).into_owned(),
        _ => String::from_utf8_lossy(d.get_bytes()).into_owned(),
    }
}

fn col_str(row: &HashMap<i64, Datum>, col_id: Option<i64>) -> String {
    col_id
        .and_then(|id| row.get(&id))
        .map(datum_string)
        .unwrap_or_default()
}

/// Converts a MySQL `SET` column string (e.g. `"Select,Insert,Update"`) into a [`PrivilegeType`]
/// bitmask (Go: `decodeSetToPrivilege` in `pkg/privilege/privileges/cache.go`).
pub fn decode_set_to_privilege(s: &str) -> PrivilegeType {
    if s.is_empty() {
        return PrivilegeType(0);
    }
    let mut ret = PrivilegeType(0);
    for elem in s.split(',') {
        let (priv_, ok) = new_priv_from_set_enum(elem);
        if !ok {
            warn!(privilege = elem, "unsupported privilege in SET column");
            continue;
        }
        ret = PrivilegeType(ret.0 | priv_.0);
    }
    ret
}

/// Resolves a privilege column name (`Select_priv`, etc.) to its [`PrivilegeType`], matching Go's
/// `mysql.Col2PrivType` with case-insensitive fallback for safety.
fn priv_from_col_name(col: &ColumnInfo) -> Option<PrivilegeType> {
    let (p, ok) = new_priv_from_column(&col.name.o);
    if ok {
        return Some(p);
    }
    // Fallback: capitalize first ASCII letter if schema stored lower-case column name.
    let mut chars = col.name.l.chars();
    let first = chars.next()?.to_ascii_uppercase();
    let title: String = std::iter::once(first).chain(chars).collect();
    let (p, ok) = new_priv_from_column(&title);
    ok.then_some(p)
}

/// Decodes scanned TiKV records of `mysql.user` into `out`.
pub fn decode_mysql_user(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let user_id = col_id("user");
    let auth_id = col_id("authentication_string");
    let locked_id = col_id("account_locked");
    let priv_cols: Vec<(i64, PrivilegeType)> = cols
        .iter()
        .filter(|c| c.field_type.get_type() == tidb_base::mysql::TypeEnum)
        .filter_map(|c| priv_from_col_name(c).map(|p| (c.id, p)))
        .collect();

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let user = col_str(&row, user_id);
        let auth_string = col_str(&row, auth_id);
        let account_locked = col_str(&row, locked_id).eq_ignore_ascii_case("Y");
        let mut privs = 0u64;
        for &(cid, p) in &priv_cols {
            if col_str(&row, Some(cid)).eq_ignore_ascii_case("Y") {
                privs |= p.0;
            }
        }
        out.add_user(
            UserRecord::new(host, user, PrivilegeType(privs))
                .with_auth_string(auth_string)
                .with_account_locked(account_locked),
        );
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.db` into `out`.
pub fn decode_mysql_db(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let db_id = col_id("db");
    let user_id = col_id("user");
    let priv_cols: Vec<(i64, PrivilegeType)> = cols
        .iter()
        .filter(|c| c.field_type.get_type() == tidb_base::mysql::TypeEnum)
        .filter_map(|c| priv_from_col_name(c).map(|p| (c.id, p)))
        .collect();

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let db = col_str(&row, db_id);
        let user = col_str(&row, user_id);
        let mut privs = 0u64;
        for &(cid, p) in &priv_cols {
            if col_str(&row, Some(cid)).eq_ignore_ascii_case("Y") {
                privs |= p.0;
            }
        }
        out.add_db(DbRecord::new(user, host, db, PrivilegeType(privs)));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.tables_priv` into `out`.
pub fn decode_mysql_tables_priv(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let db_id = col_id("db");
    let user_id = col_id("user");
    let table_name_id = col_id("table_name");
    let table_priv_id = col_id("table_priv");
    let column_priv_id = col_id("column_priv");

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let db = col_str(&row, db_id);
        let user = col_str(&row, user_id);
        let table_name = col_str(&row, table_name_id);
        let table_priv = decode_set_to_privilege(&col_str(&row, table_priv_id));
        let column_priv = decode_set_to_privilege(&col_str(&row, column_priv_id));
        out.add_table_priv(
            TablesPrivRecord::new(user, host, db, table_name, table_priv)
                .with_column_priv(column_priv),
        );
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.columns_priv` into `out`.
pub fn decode_mysql_columns_priv(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let db_id = col_id("db");
    let user_id = col_id("user");
    let table_name_id = col_id("table_name");
    let column_name_id = col_id("column_name");
    let column_priv_id = col_id("column_priv");

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let db = col_str(&row, db_id);
        let user = col_str(&row, user_id);
        let table_name = col_str(&row, table_name_id);
        let column_name = col_str(&row, column_name_id);
        let column_priv = decode_set_to_privilege(&col_str(&row, column_priv_id));
        out.add_column_priv(ColumnsPrivRecord::new(
            user,
            host,
            db,
            table_name,
            column_name,
            column_priv,
        ));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.global_grants` into `out`.
pub fn decode_mysql_global_grants(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let user_id = col_id("user");
    let priv_id = col_id("priv");
    let grant_opt_id = col_id("with_grant_option");

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let user = col_str(&row, user_id);
        let priv_name = col_str(&row, priv_id);
        let grant_option = col_str(&row, grant_opt_id).eq_ignore_ascii_case("Y");
        out.add_dynamic_priv(DynamicPrivRecord::new(user, host, priv_name, grant_option));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.default_roles` into `out`.
pub fn decode_mysql_default_roles(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let host_id = col_id("host");
    let user_id = col_id("user");
    let role_host_id = col_id("default_role_host");
    let role_user_id = col_id("default_role_user");

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let host = col_str(&row, host_id).to_lowercase();
        let user = col_str(&row, user_id);
        let role_host = col_str(&row, role_host_id);
        let role_user = col_str(&row, role_user_id);
        out.add_default_role(user, host, role_user, role_host);
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.role_edges` into `out`.
pub fn decode_mysql_role_edges(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let col_id = |name: &str| cols.iter().find(|c| c.name.l == name).map(|c| c.id);
    let from_host_id = col_id("from_host");
    let from_user_id = col_id("from_user");
    let to_host_id = col_id("to_host");
    let to_user_id = col_id("to_user");

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decode_record(tbl, k, v)?;
        let from_host = col_str(&row, from_host_id);
        let from_user = col_str(&row, from_user_id);
        let to_host = col_str(&row, to_host_id);
        let to_user = col_str(&row, to_user_id);
        out.add_role_edge(from_user, from_host, to_user, to_host);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::mysql::{
        InsertPriv, SelectPriv, SuperPriv, TypeEnum, TypeSet, TypeString, TypeVarchar, UpdatePriv,
    };
    use tidb_kv::key::{Handle as KvHandle, IntHandle, new_common_handle};
    use tidb_model::{IndexColumn, IndexInfo};
    use tidb_parser::model::CIStr;
    use tidb_privilege::{PrivilegeManager, UserPrivileges};
    use tidb_tablecodec::rowcodec::encoder::Encoder as RowEncoder;
    use tidb_types::datum::{new_mysql_enum_datum, new_mysql_set_datum, new_string_datum};
    use tidb_types::enum_type::parse_enum_name;
    use tidb_types::set_type::parse_set_name;

    fn char_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        let mut ft = FieldType::new(TypeString);
        ft.set_charset("utf8mb4");
        ft.set_collate("utf8mb4_bin");
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    }

    fn varchar_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        let mut ft = FieldType::new(TypeVarchar);
        ft.set_charset("utf8mb4");
        ft.set_collate("utf8mb4_bin");
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    }

    fn enum_yn_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        let mut ft = FieldType::new(TypeEnum);
        ft.set_elems(Some(vec![b"N".to_vec(), b"Y".to_vec()]));
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    }

    fn set_priv_col(id: i64, offset: isize, name: &str, elems: &[&str]) -> Arc<ColumnInfo> {
        let mut ft = FieldType::new(TypeSet);
        ft.set_elems(Some(elems.iter().map(|s| s.as_bytes().to_vec()).collect()));
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    }

    fn encode_v2_record(
        table_id: i64,
        row_id: i64,
        col_ids: &[i64],
        datums: &[Datum],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut enc = RowEncoder::default();
        enc.enable = true;
        let buf = enc
            .encode(Some(&utc()), col_ids, datums, None, Vec::new())
            .unwrap();
        let key = tc::encode_row_key_with_handle(table_id, &KvHandle::Int(IntHandle(row_id))).0;
        (key, buf)
    }

    fn encode_common_handle_record(
        table_id: i64,
        pk_datums: &[Datum],
        val_col_ids: &[i64],
        val_datums: &[Datum],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut encoded_pk = Vec::new();
        for d in pk_datums {
            tidb_codec::codec::encode_key(&utc(), &mut encoded_pk, std::slice::from_ref(d))
                .unwrap();
        }
        let handle = KvHandle::Common(Arc::new(new_common_handle(encoded_pk).unwrap()));
        let key = tc::encode_row_key_with_handle(table_id, &handle).0;
        let mut enc = RowEncoder::default();
        enc.enable = true;
        let buf = enc
            .encode(Some(&utc()), val_col_ids, val_datums, None, Vec::new())
            .unwrap();
        (key, buf)
    }

    #[test]
    fn decode_all_seven_mysql_privilege_tables_and_common_handle() {
        let mut mp = MySQLPrivilege::new();

        // 1. mysql.user (non-clustered v2 row, plus locked role row).
        let user_tbl = TableInfo {
            id: 5,
            name: CIStr::new("user"),
            columns: Some(vec![
                varchar_col(1, 0, "Host"),
                varchar_col(2, 1, "User"),
                varchar_col(3, 2, "authentication_string"),
                enum_yn_col(4, 3, "Select_priv"),
                enum_yn_col(5, 4, "Super_priv"),
                enum_yn_col(6, 5, "Account_locked"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let yn = &[b"N".to_vec(), b"Y".to_vec()];
        let e_y = new_mysql_enum_datum(parse_enum_name(yn, b"Y", "").unwrap());
        let e_n = new_mysql_enum_datum(parse_enum_name(yn, b"N", "").unwrap());

        let user_rows = vec![
            encode_v2_record(
                5,
                1,
                &[1, 2, 3, 4, 5, 6],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("root".to_owned()),
                    new_string_datum("*23AE809DDACAF96AF0FD78ED04B6A265E05AA257".to_owned()),
                    e_y.clone(),
                    e_y.clone(),
                    e_n.clone(),
                ],
            ),
            encode_v2_record(
                5,
                2,
                &[1, 2, 3, 4, 5, 6],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_app".to_owned()),
                    new_string_datum(String::new()),
                    e_n.clone(),
                    e_n.clone(),
                    e_n.clone(),
                ],
            ),
            encode_v2_record(
                5,
                3,
                &[1, 2, 3, 4, 5, 6],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("r_reader".to_owned()),
                    new_string_datum(String::new()),
                    e_n.clone(),
                    e_n.clone(),
                    e_y.clone(),
                ],
            ),
        ];
        decode_mysql_user(&user_tbl, &user_rows, &mut mp).unwrap();

        // 2. mysql.db (clustered common-handle CHAR PK on (Host, DB, User) where PK columns are ONLY in key!).
        let db_tbl = TableInfo {
            id: 6,
            name: CIStr::new("db"),
            is_common_handle: true,
            columns: Some(vec![
                char_col(1, 0, "Host"),
                char_col(2, 1, "DB"),
                char_col(3, 2, "User"),
                enum_yn_col(4, 3, "Select_priv"),
                enum_yn_col(5, 4, "Insert_priv"),
            ]),
            indices: Some(vec![Arc::new(IndexInfo {
                id: 1,
                name: CIStr::new("PRIMARY"),
                primary: true,
                unique: true,
                columns: Some(vec![
                    IndexColumn {
                        name: CIStr::new("Host"),
                        offset: 0,
                        length: -1,
                        ..Default::default()
                    },
                    IndexColumn {
                        name: CIStr::new("DB"),
                        offset: 1,
                        length: -1,
                        ..Default::default()
                    },
                    IndexColumn {
                        name: CIStr::new("User"),
                        offset: 2,
                        length: -1,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            })]),
            state: StatePublic,
            ..Default::default()
        };
        let db_rows = vec![encode_common_handle_record(
            6,
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("appdb".to_owned()),
                new_string_datum("u_app".to_owned()),
            ],
            &[4, 5],
            &[e_n.clone(), e_y.clone()],
        )];
        decode_mysql_db(&db_tbl, &db_rows, &mut mp).unwrap();

        // 3. mysql.tables_priv (SET column decoding).
        let tbl_priv_elems = ["Select", "Insert", "Update", "Delete"];
        let col_priv_elems = ["Select", "Insert", "Update", "References"];
        let tables_priv_tbl = TableInfo {
            id: 7,
            name: CIStr::new("tables_priv"),
            columns: Some(vec![
                varchar_col(1, 0, "Host"),
                varchar_col(2, 1, "DB"),
                varchar_col(3, 2, "User"),
                varchar_col(4, 3, "Table_name"),
                set_priv_col(5, 4, "Table_priv", &tbl_priv_elems),
                set_priv_col(6, 5, "Column_priv", &col_priv_elems),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let tbl_priv_elems_bytes: Vec<Vec<u8>> = tbl_priv_elems
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let col_priv_elems_bytes: Vec<Vec<u8>> = col_priv_elems
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
        let set_sel_upd = new_mysql_set_datum(
            parse_set_name(&tbl_priv_elems_bytes, b"Select,Update", "").unwrap(),
            "",
        );
        let set_col_sel = new_mysql_set_datum(
            parse_set_name(&col_priv_elems_bytes, b"Select", "").unwrap(),
            "",
        );
        let tp_rows = vec![encode_v2_record(
            7,
            1,
            &[1, 2, 3, 4, 5, 6],
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("test".to_owned()),
                new_string_datum("r_reader".to_owned()),
                new_string_datum("t_role".to_owned()),
                set_sel_upd,
                set_col_sel,
            ],
        )];
        decode_mysql_tables_priv(&tables_priv_tbl, &tp_rows, &mut mp).unwrap();

        // 4. mysql.columns_priv.
        let columns_priv_tbl = TableInfo {
            id: 8,
            name: CIStr::new("columns_priv"),
            columns: Some(vec![
                varchar_col(1, 0, "Host"),
                varchar_col(2, 1, "DB"),
                varchar_col(3, 2, "User"),
                varchar_col(4, 3, "Table_name"),
                varchar_col(5, 4, "Column_name"),
                set_priv_col(6, 5, "Column_priv", &col_priv_elems),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let cp_rows = vec![encode_v2_record(
            8,
            1,
            &[1, 2, 3, 4, 5, 6],
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("test".to_owned()),
                new_string_datum("u_app".to_owned()),
                new_string_datum("t".to_owned()),
                new_string_datum("a".to_owned()),
                new_mysql_set_datum(
                    parse_set_name(&col_priv_elems_bytes, b"Select", "").unwrap(),
                    "",
                ),
            ],
        )];
        decode_mysql_columns_priv(&columns_priv_tbl, &cp_rows, &mut mp).unwrap();

        // 5. mysql.global_grants.
        let gg_tbl = TableInfo {
            id: 9,
            name: CIStr::new("global_grants"),
            columns: Some(vec![
                varchar_col(1, 0, "Host"),
                varchar_col(2, 1, "User"),
                varchar_col(3, 2, "Priv"),
                enum_yn_col(4, 3, "With_Grant_Option"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let gg_rows = vec![encode_v2_record(
            9,
            1,
            &[1, 2, 3, 4],
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("u_app".to_owned()),
                new_string_datum("BACKUP_ADMIN".to_owned()),
                e_y.clone(),
            ],
        )];
        decode_mysql_global_grants(&gg_tbl, &gg_rows, &mut mp).unwrap();

        // 6 & 7. mysql.role_edges + mysql.default_roles.
        let re_tbl = TableInfo {
            id: 10,
            name: CIStr::new("role_edges"),
            columns: Some(vec![
                varchar_col(1, 0, "FROM_HOST"),
                varchar_col(2, 1, "FROM_USER"),
                varchar_col(3, 2, "TO_HOST"),
                varchar_col(4, 3, "TO_USER"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let re_rows = vec![encode_v2_record(
            10,
            1,
            &[1, 2, 3, 4],
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("r_reader".to_owned()),
                new_string_datum("%".to_owned()),
                new_string_datum("u_app".to_owned()),
            ],
        )];
        decode_mysql_role_edges(&re_tbl, &re_rows, &mut mp).unwrap();

        let dr_tbl = TableInfo {
            id: 11,
            name: CIStr::new("default_roles"),
            columns: Some(vec![
                varchar_col(1, 0, "HOST"),
                varchar_col(2, 1, "USER"),
                varchar_col(3, 2, "DEFAULT_ROLE_HOST"),
                varchar_col(4, 3, "DEFAULT_ROLE_USER"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let dr_rows = vec![encode_v2_record(
            11,
            1,
            &[1, 2, 3, 4],
            &[
                new_string_datum("%".to_owned()),
                new_string_datum("u_app".to_owned()),
                new_string_datum("%".to_owned()),
                new_string_datum("r_reader".to_owned()),
            ],
        )];
        decode_mysql_default_roles(&dr_tbl, &dr_rows, &mut mp).unwrap();

        let handle = Arc::new(Handle::new(mp));
        let snap = handle.get();

        // Verify mysql.user auth_string, privileges, and Account_locked.
        let root_rec = snap.match_user("root", "127.0.0.1").unwrap();
        assert_eq!(
            root_rec.auth_string,
            "*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"
        );
        assert!(!root_rec.account_locked);
        assert_eq!(
            root_rec.privileges.0 & (SelectPriv.0 | SuperPriv.0),
            SelectPriv.0 | SuperPriv.0
        );

        let role_rec = snap.match_user("r_reader", "127.0.0.1").unwrap();
        assert!(role_rec.account_locked);

        // Verify common-handle mysql.db decoding.
        let u_app = UserPrivileges::new("u_app", "%", Arc::clone(&handle));
        let default_roles = snap.get_default_roles("u_app", "%");
        assert_eq!(default_roles.len(), 1);
        assert_eq!(default_roles[0].username, "r_reader");

        assert!(u_app.request_verification(&[], "appdb", "x", "", InsertPriv));
        assert!(!u_app.request_verification(&[], "appdb", "x", "*", SelectPriv));
        assert!(u_app.db_is_visible(&[], "appdb"));

        // Verify mysql.columns_priv decoding.
        assert!(u_app.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(u_app.request_verification(&[], "test", "t", "a", SelectPriv));
        assert!(!u_app.request_verification(&[], "test", "t", "b", SelectPriv));

        // Verify mysql.tables_priv via default role edge r_reader -> u_app.
        assert!(!u_app.request_verification(&[], "test", "t_role", "*", SelectPriv));
        assert!(u_app.request_verification(&default_roles, "test", "t_role", "*", SelectPriv));
        assert!(u_app.request_verification(&default_roles, "test", "t_role", "", UpdatePriv));

        // Verify mysql.global_grants.
        assert!(u_app.request_dynamic_verification(&[], "BACKUP_ADMIN", true));
    }
}
