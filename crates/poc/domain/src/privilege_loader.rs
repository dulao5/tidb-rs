//! Cluster-backed `mysql.*` privilege table loading and live etcd `/tidb/privilege` reload
//! (D04.1 Part 5/5; Go: `pkg/privilege/privileges/cache.go`, `pkg/domain/domain.go`).
//!
//! Reads the eight MySQL privilege tables directly from TiKV at a single consistent TSO snapshot
//! using `tidb-tablecodec`:
//! - `mysql.user` (required)
//! - `mysql.global_priv` (TLS/SSL/X509/SAN requirements)
//! - `mysql.db`
//! - `mysql.tables_priv`
//! - `mysql.columns_priv`
//! - `mysql.global_grants`
//! - `mysql.default_roles`
//! - `mysql.role_edges`
//!
//! Startup fails fast if the initial load fails (never silently falling back to open access),
//! and corrupted `TableInfo` JSON or missing required columns are hard errors. Subsequent
//! background reloads mirror Go's `LoadPrivilegeLoop` (`pkg/domain/domain.go:1927, 1971-1975`):
//! - Reload once immediately whenever the etcd `/tidb/privilege` watch is established or
//!   reconnected (closing the startup-load-to-watch and reconnect/compaction windows).
//! - Coalesce bursts of `/tidb/privilege` watch events for 5 ms (up to 128 events) before
//!   running one reload.
//! - Periodic fallback refresh every 10 minutes when etcd is configured (5 minutes without
//!   etcd), resetting the timer after each watch-triggered reload.
//! - Transient runtime reload errors log a warning and preserve the last known good snapshot.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use etcd_client::{Client as EtcdClient, ConnectOptions, WatchOptions};
use poc_kv::{KeyRange, KvClient};
use tidb_base::gostd::time::{Location, utc};
use tidb_base::mysql::{PrivilegeType, new_priv_from_column, new_priv_from_set_enum};
use tidb_meta::keys as meta_keys;
use tidb_model::{ColumnInfo, IndexInfo, StatePublic, TableInfo};
use tidb_parser::types::FieldType;
use tidb_privilege::{
    ColumnsPrivRecord, DbRecord, DynamicPrivRecord, GlobalPrivRecord, Handle, MySQLPrivilege,
    TablesPrivRecord, UserRecord,
};
use tidb_systable::session_io::{MYSQL_DB_ID, SYS_VAR_CACHE_KEY, SYS_VAR_CACHE_PERIOD};
use tidb_tablecodec::tablecodec as tc;
use tidb_types::datum::{
    Datum, KindInt64, KindMysqlEnum, KindMysqlJSON, KindMysqlSet, KindMysqlTime, KindString,
    KindUint64,
};
use tokio::sync::Notify;

use crate::{Error, Result, keys};

/// Periodic fallback reload interval with etcd watch active (Go `domain.go:1927`: 10 min).
const PRIVILEGE_REFRESH_TICK_WITH_ETCD: Duration = Duration::from_secs(600);
/// Periodic fallback reload interval without etcd watch (Go `domain.go:1927`: 5 min).
const PRIVILEGE_REFRESH_TICK_NO_ETCD: Duration = Duration::from_secs(300);
/// Watch event batching window and max batch size (Go `domain.go:1971-1975`: 5 ms, 128 events).
const WATCH_BATCH_WINDOW: Duration = Duration::from_millis(5);
const WATCH_BATCH_MAX: usize = 128;
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
    /// Notified after each watch-triggered privilege reload so the periodic timer resets.
    reset_tick: Notify,
    /// Notified after each sysvar/privilege reload so the 30s `default_password_lifetime` timer resets.
    reset_sysvar_tick: Notify,
    stop: tokio::sync::watch::Sender<bool>,
}

impl PrivilegeSync {
    /// Performs the initial load of `mysql.*` privilege tables from TiKV (failing fast on error)
    /// and starts the background etcd `/tidb/privilege` + `/tidb/sysvars` watchers and periodic
    /// refresh loops.
    pub async fn start(kv: KvClient, pd_endpoints: Vec<String>) -> Result<Self> {
        let initial = load_privileges_from_kv(&kv).await?;
        let handle = Arc::new(Handle::new(initial));
        let (stop, _) = tokio::sync::watch::channel(false);
        let has_etcd = !pd_endpoints.is_empty();
        let inner = Arc::new(Inner {
            kv,
            handle,
            reload_lock: tokio::sync::Mutex::new(()),
            reset_tick: Notify::new(),
            reset_sysvar_tick: Notify::new(),
            stop,
        });
        if has_etcd {
            tokio::spawn(inner.clone().watch_loop(pd_endpoints));
        }
        let tick = if has_etcd {
            PRIVILEGE_REFRESH_TICK_WITH_ETCD
        } else {
            PRIVILEGE_REFRESH_TICK_NO_ETCD
        };
        tokio::spawn(inner.clone().periodic_refresh_loop(tick));
        tokio::spawn(
            inner
                .clone()
                .periodic_sysvar_refresh_loop(SYS_VAR_CACHE_PERIOD),
        );
        tidb_base::log_info!("cluster privilege cache loaded from mysql.*");
        Ok(Self { inner })
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

    /// Lightweight refresh of `@@global.default_password_lifetime` from `mysql.global_variables`,
    /// triggered by etcd `/tidb/sysvars` (`SET GLOBAL`) notifications and the 30s fallback tick.
    async fn reload_default_password_lifetime(&self) -> Result<()> {
        let _guard = self.reload_lock.lock().await;
        let next = load_default_password_lifetime_from_kv(&self.kv).await?;
        let cur = self.handle.get();
        if cur.default_password_lifetime() == next {
            return Ok(());
        }
        let mut updated = (*cur).clone();
        updated.set_default_password_lifetime_opt(next);
        self.handle.replace(updated);
        Ok(())
    }

    async fn periodic_refresh_loop(self: Arc<Self>, tick: Duration) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            tokio::select! {
                _ = tokio::time::sleep(tick) => {
                    if self.stopped() {
                        return;
                    }
                    if let Err(e) = self.reload().await {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "periodic privilege reload failed; keeping previous snapshot",
                            "error" => err_str.as_str(),
                        );
                    }
                }
                _ = self.reset_tick.notified() => {
                    // Watch reload succeeded; reset periodic countdown (Go `domain.go:1985`).
                }
                _ = stop.changed() => return,
            }
        }
    }

    async fn periodic_sysvar_refresh_loop(self: Arc<Self>, tick: Duration) {
        let mut stop = self.stop.subscribe();
        while !self.stopped() {
            tokio::select! {
                _ = tokio::time::sleep(tick) => {
                    if self.stopped() {
                        return;
                    }
                    if let Err(e) = self.reload_default_password_lifetime().await {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "periodic default_password_lifetime reload failed; keeping previous value",
                            "error" => err_str.as_str(),
                        );
                    }
                }
                _ = self.reset_sysvar_tick.notified() => {
                    // Watch or full privilege reload refreshed global_variables; reset 30s countdown.
                }
                _ = stop.changed() => return,
            }
        }
    }

    async fn watch_loop(self: Arc<Self>, pd_endpoints: Vec<String>) {
        let mut stop = self.stop.subscribe();
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        while !self.stopped() {
            match self
                .watch_once(&pd_endpoints, &mut stop, &mut backoff)
                .await
            {
                Ok(()) => return,
                Err(e) => {
                    let err_str = e.to_string();
                    tidb_base::log_warn!(
                        "privilege etcd watch failed; retrying with backoff",
                        "error" => err_str.as_str(),
                        "retry_in_secs" => backoff.as_secs(),
                    );
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = stop.changed() => return,
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn watch_once(
        &self,
        pd_endpoints: &[String],
        stop: &mut tokio::sync::watch::Receiver<bool>,
        backoff: &mut Duration,
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
        let (_priv_watcher, mut priv_stream) = tokio::time::timeout(
            ETCD_OP_TIMEOUT,
            etcd.watch(
                keys::PRIVILEGE_NOTIFY,
                Some(WatchOptions::new().with_start_revision(revision + 1)),
            ),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        let (_sysvar_watcher, mut sysvar_stream) = tokio::time::timeout(
            ETCD_OP_TIMEOUT,
            etcd.watch(
                SYS_VAR_CACHE_KEY,
                Some(WatchOptions::new().with_start_revision(revision + 1)),
            ),
        )
        .await
        .map_err(|_| Error::Timeout)??;

        // Reload once right after establishing/reconnecting both watch streams so any GRANT/REVOKE
        // or SET GLOBAL default_password_lifetime committed between startup (or a prior watch
        // error/compaction) and `revision + 1` is never missed.
        if let Err(e) = self.reload().await {
            let err_str = e.to_string();
            tidb_base::log_warn!(
                "privilege reload on watch connect failed; keeping previous snapshot",
                "error" => err_str.as_str(),
            );
        } else {
            *backoff = Duration::from_secs(1);
            self.reset_tick.notify_one();
            self.reset_sysvar_tick.notify_one();
        }

        enum WatchEvent {
            Privilege(Option<etcd_client::WatchResponse>),
            SysVar(Option<etcd_client::WatchResponse>),
            Stopped,
        }

        loop {
            let ev = tokio::select! {
                m = priv_stream.message() => WatchEvent::Privilege(m?),
                m = sysvar_stream.message() => WatchEvent::SysVar(m?),
                _ = stop.changed() => WatchEvent::Stopped,
            };
            match ev {
                WatchEvent::Stopped => return Ok(()),
                WatchEvent::Privilege(msg) => {
                    let Some(msg) = msg else {
                        return Err(Error::Other("privilege watch stream ended".to_owned()));
                    };
                    if msg.canceled() {
                        return Err(Error::Other("privilege watch canceled".to_owned()));
                    }
                    if msg.events().is_empty() {
                        continue;
                    }
                    let batched =
                        drain_watch_burst(&mut priv_stream, msg.events().len(), "privilege")
                            .await?;
                    tidb_base::log_debug!(
                        "received /tidb/privilege notification; reloading privilege cache",
                        "events" => batched as u64,
                    );
                    if let Err(e) = self.reload().await {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "privilege reload on etcd notification failed; keeping previous snapshot",
                            "error" => err_str.as_str(),
                        );
                    } else {
                        self.reset_tick.notify_one();
                        self.reset_sysvar_tick.notify_one();
                    }
                }
                WatchEvent::SysVar(msg) => {
                    let Some(msg) = msg else {
                        return Err(Error::Other("sysvar watch stream ended".to_owned()));
                    };
                    if msg.canceled() {
                        return Err(Error::Other("sysvar watch canceled".to_owned()));
                    }
                    if msg.events().is_empty() {
                        continue;
                    }
                    let batched =
                        drain_watch_burst(&mut sysvar_stream, msg.events().len(), "sysvar").await?;
                    tidb_base::log_debug!(
                        "received /tidb/sysvars notification; reloading default_password_lifetime",
                        "events" => batched as u64,
                    );
                    if let Err(e) = self.reload_default_password_lifetime().await {
                        let err_str = e.to_string();
                        tidb_base::log_warn!(
                            "default_password_lifetime reload on etcd notification failed; keeping previous value",
                            "error" => err_str.as_str(),
                        );
                    } else {
                        self.reset_sysvar_tick.notify_one();
                    }
                }
            }
        }
    }
}

/// Coalesces burst watch notifications for up to 5 ms / 128 events (Go `domain.go:1971-1975`).
async fn drain_watch_burst(
    stream: &mut etcd_client::WatchStream,
    initial: usize,
    kind: &str,
) -> Result<usize> {
    let mut batched = initial;
    let deadline = tokio::time::Instant::now() + WATCH_BATCH_WINDOW;
    while batched < WATCH_BATCH_MAX {
        let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.message()).await {
            Ok(Ok(Some(more))) => {
                if more.canceled() {
                    return Err(Error::Other(format!("{kind} watch canceled")));
                }
                batched += more.events().len();
            }
            Ok(Ok(None)) => {
                return Err(Error::Other(format!("{kind} watch stream ended")));
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => break, // batch window elapsed
        }
    }
    Ok(batched)
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

    // Remaining tables (7 privilege tables + `mysql.global_variables` for
    // `default_password_lifetime`): load if present; fail the whole snapshot on decode/KV error.
    type DecoderFn = fn(&TableInfo, &[(Vec<u8>, Vec<u8>)], &mut MySQLPrivilege) -> Result<()>;
    for (name, decoder) in [
        ("global_priv", decode_mysql_global_priv as DecoderFn),
        ("db", decode_mysql_db),
        ("tables_priv", decode_mysql_tables_priv),
        ("columns_priv", decode_mysql_columns_priv),
        ("global_grants", decode_mysql_global_grants),
        ("default_roles", decode_mysql_default_roles),
        ("role_edges", decode_mysql_role_edges),
        ("global_variables", decode_mysql_global_variables),
    ] {
        match tables.get(name) {
            Some(tbl) => {
                let rows = scan_table_records(kv, ts, tbl.id).await?;
                decoder(tbl, &rows, &mut mp)?;
            }
            None => {
                tidb_base::log_warn!(
                    "optional mysql privilege table not found in metadata",
                    "table" => name,
                );
            }
        }
    }

    mp.sort_all();
    Ok(mp)
}

/// Lowercase names of `mysql.*` tables actually loaded by [`load_privileges_from_kv`].
/// Unrelated `mysql.*` system tables (e.g. `stats_*`, `bind_info`) are skipped before full
/// `TableInfo::unmarshal_json` so schema extensions on unrelated tables never break privilege sync.
const LOADED_MYSQL_TABLES: &[&str] = &[
    "user",
    "global_priv",
    "db",
    "tables_priv",
    "columns_priv",
    "global_grants",
    "default_roles",
    "role_edges",
    "global_variables",
];

fn peek_mysql_table_lower_name(raw: &[u8]) -> Option<String> {
    let val: serde_json::Value = serde_json::from_slice(raw).ok()?;
    val.get("name")
        .and_then(|n| n.get("L"))
        .and_then(|s| s.as_str())
        .map(str::to_lowercase)
}

/// Loads only `@@global.default_password_lifetime` from `mysql.global_variables` at a fresh TSO.
pub async fn load_default_password_lifetime_from_kv(kv: &KvClient) -> Result<Option<u64>> {
    let ts = kv.current_ts().await?;
    let tables = load_mysql_table_infos_filtered(kv, ts, &["global_variables"]).await?;
    let Some(tbl) = tables.get("global_variables") else {
        return Ok(Some(0));
    };
    let rows = scan_table_records(kv, ts, tbl.id).await?;
    parse_default_password_lifetime(tbl, &rows)
}

/// Reads `TableInfo` entries for [`LOADED_MYSQL_TABLES`] under `mysql` DB metadata at `ts`.
/// Fails hard on corrupted JSON for any target table rather than silently treating it as missing.
async fn load_mysql_table_infos(kv: &KvClient, ts: u64) -> Result<HashMap<String, TableInfo>> {
    load_mysql_table_infos_filtered(kv, ts, LOADED_MYSQL_TABLES).await
}

pub(crate) async fn load_mysql_table_infos_filtered(
    kv: &KvClient,
    ts: u64,
    wanted: &[&str],
) -> Result<HashMap<String, TableInfo>> {
    let prefix = meta_keys::hash_data_prefix(&meta_keys::db_key(MYSQL_DB_ID));
    let range = KeyRange::new(prefix.clone(), meta_keys::prefix_next(&prefix));
    let pairs = kv.snapshot(ts).scan_all(range).await?;
    let mut out = HashMap::new();
    for (k, v) in pairs {
        let Some(field) = meta_keys::decode_hash_field(&k, &prefix) else {
            continue;
        };
        let Some(table_id) = meta_keys::parse_table_key(&field) else {
            continue;
        };
        if let Some(lower_name) = peek_mysql_table_lower_name(&v)
            && !wanted.contains(&lower_name.as_str())
        {
            continue;
        }
        let mut t = TableInfo::default();
        t.unmarshal_json(&v).map_err(|e| {
            Error::Other(format!(
                "corrupted TableInfo JSON in mysql meta (table_id={table_id}): {e}"
            ))
        })?;
        if t.state == StatePublic && wanted.contains(&t.name.l.as_str()) {
            out.insert(t.name.l.clone(), t);
        }
    }
    Ok(out)
}

/// Scans all record keys `t<table_id>_r...` of `table_id` at snapshot `ts`.
pub(crate) async fn scan_table_records(
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

/// Precomputed table metadata for decoding multiple rows of one `mysql.*` table without
/// rebuilding the column type map on every row.
struct RowDecoder<'a> {
    table_name: &'a str,
    col_types: HashMap<i64, &'a FieldType>,
    handle_col_ids: Vec<i64>,
    loc: Arc<Location>,
}

impl<'a> RowDecoder<'a> {
    fn new(tbl: &'a TableInfo) -> Self {
        let cols = tbl.columns.as_deref().unwrap_or(&[]);
        let col_types = cols.iter().map(|c| (c.id, &c.field_type)).collect();
        Self {
            table_name: &tbl.name.l,
            col_types,
            handle_col_ids: handle_col_ids(tbl),
            loc: utc(),
        }
    }

    fn decode(&self, key: &[u8], val: &[u8]) -> Result<HashMap<i64, Datum>> {
        let row = tc::decode_row_to_datum_map(val, &self.col_types, Some(&self.loc))
            .map_err(|e| Error::Other(format!("decode row of mysql.{}: {e}", self.table_name)))?;
        if self.handle_col_ids.is_empty() {
            return Ok(row);
        }
        let handle = tc::decode_row_key(key).map_err(|e| {
            Error::Other(format!("decode handle of mysql.{}: {e}", self.table_name))
        })?;
        let (merged, res) = tc::decode_handle_to_datum_map(
            Some(&handle),
            &self.handle_col_ids,
            &self.col_types,
            Some(&self.loc),
            Some(row),
        );
        res.map_err(|e| {
            Error::Other(format!(
                "decode handle datums of mysql.{}: {e}",
                self.table_name
            ))
        })?;
        Ok(merged)
    }
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

fn require_col(tbl: &TableInfo, name_lower: &str) -> Result<i64> {
    tbl.columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|c| c.name.l == name_lower)
        .map(|c| c.id)
        .ok_or_else(|| {
            Error::Other(format!(
                "required column '{name_lower}' missing from mysql.{}",
                tbl.name.l
            ))
        })
}

fn optional_col(tbl: &TableInfo, name_lower: &str) -> Option<i64> {
    tbl.columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .find(|c| c.name.l == name_lower)
        .map(|c| c.id)
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
        KindMysqlJSON => String::from_utf8_lossy(&d.get_mysql_json().string()).into_owned(),
        _ => String::from_utf8_lossy(d.get_bytes()).into_owned(),
    }
}

fn col_str(row: &HashMap<i64, Datum>, col_id: Option<i64>) -> String {
    col_id
        .and_then(|id| row.get(&id))
        .map(datum_string)
        .unwrap_or_default()
}

/// Decodes `mysql.user.password_last_changed` (`TIMESTAMP`, decoded in UTC by [`RowDecoder`])
/// into UTC Unix seconds. Returns `None` for SQL `NULL`, zero timestamps (`0000-00-00 00:00:00`),
/// missing column, or invalid/unexpected datum types (which fail closed when effective lifetime > 0).
#[allow(non_upper_case_globals)]
fn decode_password_last_changed(row: &HashMap<i64, Datum>, col_id: Option<i64>) -> Option<i64> {
    let d = col_id.and_then(|id| row.get(&id))?;
    if d.is_null() {
        return None;
    }
    match d.kind() {
        KindMysqlTime => {
            let t = d.get_mysql_time();
            if t.is_zero() {
                return None;
            }
            let (gt, err) = t.go_time(&utc());
            if err.is_err() {
                return None;
            }
            Some(gt.unix())
        }
        _ => None,
    }
}

/// Decodes `mysql.user.password_lifetime` into `(lifetime_days, invalid)`.
/// When the column holds an unexpected non-null datum type, returns `(None, true)` so
/// `is_password_expired_at` fails closed rather than treating it as a huge valid lifetime.
#[allow(non_upper_case_globals)]
fn decode_password_lifetime(row: &HashMap<i64, Datum>, col_id: Option<i64>) -> (Option<u16>, bool) {
    let Some(d) = col_id.and_then(|id| row.get(&id)) else {
        return (None, false);
    };
    if d.is_null() {
        return (None, false);
    }
    match d.kind() {
        KindInt64 => {
            let v = d.get_int64();
            if v <= 0 {
                (Some(0), false)
            } else {
                (Some(u16::try_from(v).unwrap_or(u16::MAX)), false)
            }
        }
        KindUint64 => (
            Some(u16::try_from(d.get_uint64()).unwrap_or(u16::MAX)),
            false,
        ),
        _ => (None, true),
    }
}

/// Checks whether `mysql.user.User_attributes` JSON carries a failed-login lockout policy or
/// auto-locked state (`$.Password_locking.{failed_login_attempts,password_lock_time_days,
/// auto_account_locked}`). Unparseable non-empty JSON fails closed (`true`).
fn has_failed_login_policy_in_attrs(row: &HashMap<i64, Datum>, col_id: Option<i64>) -> bool {
    let Some(d) = col_id.and_then(|id| row.get(&id)) else {
        return false;
    };
    if d.is_null() {
        return false;
    }
    let raw = datum_string(d);
    if raw.trim().is_empty() {
        return false;
    }
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return true; // fail closed on unparseable JSON
    };
    let Some(locking) = val.get("Password_locking") else {
        return false;
    };
    if locking.is_null() {
        return false;
    }
    let Some(obj) = locking.as_object() else {
        return true;
    };
    let attempts = obj
        .get("failed_login_attempts")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let lock_days = obj
        .get("password_lock_time_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let auto_locked = match obj.get("auto_account_locked") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("Y") || s == "1",
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::Null) | None => false,
        Some(_) => true,
    };
    attempts > 0 || lock_days != 0 || auto_locked
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
            tidb_base::log_warn!(
                "unsupported privilege in SET column",
                "privilege" => elem,
            );
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
    let host_id = require_col(tbl, "host")?;
    let user_id = require_col(tbl, "user")?;
    let auth_id = require_col(tbl, "authentication_string")?;
    let locked_id = optional_col(tbl, "account_locked");
    let plugin_id = optional_col(tbl, "plugin");
    let pwd_expired_id = optional_col(tbl, "password_expired");
    let pwd_last_changed_id = optional_col(tbl, "password_last_changed");
    let pwd_lifetime_id = optional_col(tbl, "password_lifetime");
    let user_attrs_id = optional_col(tbl, "user_attributes");
    let priv_cols: Vec<(i64, PrivilegeType)> = tbl
        .columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|c| c.field_type.get_type() == tidb_base::mysql::TypeEnum)
        .filter_map(|c| priv_from_col_name(c).map(|p| (c.id, p)))
        .collect();
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let user = col_str(&row, Some(user_id));
        let auth_string = col_str(&row, Some(auth_id));
        let plugin = col_str(&row, plugin_id);
        let account_locked = col_str(&row, locked_id).eq_ignore_ascii_case("Y");
        let password_expired = col_str(&row, pwd_expired_id).eq_ignore_ascii_case("Y");
        let password_last_changed = decode_password_last_changed(&row, pwd_last_changed_id);
        let (password_lifetime, password_lifetime_invalid) =
            decode_password_lifetime(&row, pwd_lifetime_id);
        let has_failed_login_policy = has_failed_login_policy_in_attrs(&row, user_attrs_id);
        let mut privs = 0u64;
        for &(cid, p) in &priv_cols {
            if col_str(&row, Some(cid)).eq_ignore_ascii_case("Y") {
                privs |= p.0;
            }
        }
        out.push_user(
            UserRecord::new(host, user, PrivilegeType(privs))
                .with_auth_string(auth_string)
                .with_auth_plugin(plugin)
                .with_account_locked(account_locked)
                .with_password_expired(password_expired)
                .with_password_last_changed(password_last_changed)
                .with_password_lifetime(password_lifetime)
                .with_password_lifetime_invalid(password_lifetime_invalid)
                .with_failed_login_policy(has_failed_login_policy),
        );
    }
    Ok(())
}

/// Parses `default_password_lifetime` from scanned `mysql.global_variables` records.
/// Returns `Some(0)` if the variable is absent or empty, `Some(days)` if valid, and `None`
/// (fail-closed) if the stored value is non-empty and unparseable.
fn parse_default_password_lifetime(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
) -> Result<Option<u64>> {
    let name_id = require_col(tbl, "variable_name")?;
    let val_id = require_col(tbl, "variable_value")?;
    let decoder = RowDecoder::new(tbl);
    let mut parsed = Some(0);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let name = col_str(&row, Some(name_id));
        if !name.eq_ignore_ascii_case("default_password_lifetime") {
            continue;
        }
        let val = col_str(&row, Some(val_id));
        let trimmed = val.trim();
        parsed = if trimmed.is_empty() {
            Some(0)
        } else {
            match trimmed.parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    tidb_base::log_warn!(
                        "unparseable default_password_lifetime in mysql.global_variables; failing closed",
                        "value" => trimmed,
                    );
                    None
                }
            }
        };
    }
    Ok(parsed)
}

/// Decodes `default_password_lifetime` from `mysql.global_variables` into `out`.
///
/// Accounts whose `mysql.user.password_lifetime` is `NULL` (the `CREATE USER` default) inherit
/// `@@global.default_password_lifetime`. Unparseable non-empty values fail closed (`None`).
pub fn decode_mysql_global_variables(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let parsed = parse_default_password_lifetime(tbl, records)?;
    out.set_default_password_lifetime_opt(parsed);
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.global_priv` into `out` (Go `decodeGlobalPrivTableRow`).
pub fn decode_mysql_global_priv(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let host_id = require_col(tbl, "host")?;
    let user_id = require_col(tbl, "user")?;
    let priv_id = require_col(tbl, "priv")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let user = col_str(&row, Some(user_id));
        let priv_data = col_str(&row, Some(priv_id));
        let (require_ssl, broken) = if priv_data.is_empty() {
            (false, false)
        } else {
            match serde_json::from_str::<serde_json::Value>(&priv_data) {
                Ok(serde_json::Value::Object(map)) => {
                    // Go `SSLType`: -1 = NotSpecified, 0 = None, 1 = Any (SSL), 2 = X509,
                    // 3 = Specified (CIPHER/ISSUER/SUBJECT/SAN).
                    let ssl_type = map.get("ssl_type").and_then(|v| v.as_i64()).unwrap_or(0);
                    let non_empty_str = |key: &str| {
                        map.get(key)
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                    };
                    let req = ssl_type > 0
                        || non_empty_str("ssl_cipher")
                        || non_empty_str("x509_issuer")
                        || non_empty_str("x509_subject")
                        || non_empty_str("san");
                    (req, false)
                }
                _ => {
                    tidb_base::log_warn!(
                        "mysql.global_priv JSON is broken; forbidding login until fixed",
                        "user" => user.as_str(),
                        "host" => host.as_str(),
                    );
                    (true, true)
                }
            }
        };
        out.push_global_priv(GlobalPrivRecord::new(user, host, require_ssl, broken));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.db` into `out`.
pub fn decode_mysql_db(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let host_id = require_col(tbl, "host")?;
    let db_id = require_col(tbl, "db")?;
    let user_id = require_col(tbl, "user")?;
    let priv_cols: Vec<(i64, PrivilegeType)> = tbl
        .columns
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|c| c.field_type.get_type() == tidb_base::mysql::TypeEnum)
        .filter_map(|c| priv_from_col_name(c).map(|p| (c.id, p)))
        .collect();
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let db = col_str(&row, Some(db_id));
        let user = col_str(&row, Some(user_id));
        let mut privs = 0u64;
        for &(cid, p) in &priv_cols {
            if col_str(&row, Some(cid)).eq_ignore_ascii_case("Y") {
                privs |= p.0;
            }
        }
        out.push_db(DbRecord::new(user, host, db, PrivilegeType(privs)));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.tables_priv` into `out`.
pub fn decode_mysql_tables_priv(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let host_id = require_col(tbl, "host")?;
    let db_id = require_col(tbl, "db")?;
    let user_id = require_col(tbl, "user")?;
    let table_name_id = require_col(tbl, "table_name")?;
    let table_priv_id = require_col(tbl, "table_priv")?;
    let column_priv_id = require_col(tbl, "column_priv")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let db = col_str(&row, Some(db_id));
        let user = col_str(&row, Some(user_id));
        let table_name = col_str(&row, Some(table_name_id));
        let table_priv = decode_set_to_privilege(&col_str(&row, Some(table_priv_id)));
        let column_priv = decode_set_to_privilege(&col_str(&row, Some(column_priv_id)));
        out.push_table_priv(
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
    let host_id = require_col(tbl, "host")?;
    let db_id = require_col(tbl, "db")?;
    let user_id = require_col(tbl, "user")?;
    let table_name_id = require_col(tbl, "table_name")?;
    let column_name_id = require_col(tbl, "column_name")?;
    let column_priv_id = require_col(tbl, "column_priv")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let db = col_str(&row, Some(db_id));
        let user = col_str(&row, Some(user_id));
        let table_name = col_str(&row, Some(table_name_id));
        let column_name = col_str(&row, Some(column_name_id));
        let column_priv = decode_set_to_privilege(&col_str(&row, Some(column_priv_id)));
        out.push_column_priv(ColumnsPrivRecord::new(
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
    let host_id = require_col(tbl, "host")?;
    let user_id = require_col(tbl, "user")?;
    let priv_id = require_col(tbl, "priv")?;
    let grant_opt_id = require_col(tbl, "with_grant_option")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let user = col_str(&row, Some(user_id));
        let priv_name = col_str(&row, Some(priv_id));
        let grant_option = col_str(&row, Some(grant_opt_id)).eq_ignore_ascii_case("Y");
        out.push_dynamic_priv(DynamicPrivRecord::new(user, host, priv_name, grant_option));
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.default_roles` into `out`.
pub fn decode_mysql_default_roles(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let host_id = require_col(tbl, "host")?;
    let user_id = require_col(tbl, "user")?;
    let role_host_id = require_col(tbl, "default_role_host")?;
    let role_user_id = require_col(tbl, "default_role_user")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let host = col_str(&row, Some(host_id)).to_lowercase();
        let user = col_str(&row, Some(user_id));
        let role_host = col_str(&row, Some(role_host_id)).to_lowercase();
        let role_user = col_str(&row, Some(role_user_id));
        out.push_default_role(user, host, role_user, role_host);
    }
    Ok(())
}

/// Decodes scanned TiKV records of `mysql.role_edges` into `out`.
pub fn decode_mysql_role_edges(
    tbl: &TableInfo,
    records: &[(Vec<u8>, Vec<u8>)],
    out: &mut MySQLPrivilege,
) -> Result<()> {
    let from_host_id = require_col(tbl, "from_host")?;
    let from_user_id = require_col(tbl, "from_user")?;
    let to_host_id = require_col(tbl, "to_host")?;
    let to_user_id = require_col(tbl, "to_user")?;
    let decoder = RowDecoder::new(tbl);

    for (k, v) in records {
        if !tc::is_record_key(k) {
            continue;
        }
        let row = decoder.decode(k, v)?;
        let from_host = col_str(&row, Some(from_host_id)).to_lowercase();
        let from_user = col_str(&row, Some(from_user_id));
        let to_host = col_str(&row, Some(to_host_id)).to_lowercase();
        let to_user = col_str(&row, Some(to_user_id));
        out.add_role_edge(from_user, from_host, to_user, to_host);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidb_base::mysql::{
        InsertPriv, SelectPriv, SuperPriv, TypeEnum, TypeJSON, TypeSet, TypeShort, TypeString,
        TypeTimestamp, TypeVarchar, UnsignedFlag, UpdatePriv,
    };
    use tidb_kv::key::{Handle as KvHandle, IntHandle, new_common_handle};
    use tidb_model::{IndexColumn, IndexInfo};
    use tidb_parser::model::CIStr;
    use tidb_privilege::{PrivilegeManager, UserPrivileges};
    use tidb_tablecodec::rowcodec::encoder::Encoder as RowEncoder;
    use tidb_types::core_time::ZeroCoreTime;
    use tidb_types::datum::{
        new_mysql_enum_datum, new_mysql_set_datum, new_string_datum, new_time_datum, new_uint_datum,
    };
    use tidb_types::enum_type::parse_enum_name;
    use tidb_types::json::parse_binary_json_from_string;
    use tidb_types::set_type::parse_set_name;
    use tidb_types::time::{from_date_checked, new_time};

    fn timestamp_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: FieldType::new(TypeTimestamp),
            state: StatePublic,
            ..Default::default()
        })
    }

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

    fn smallint_unsigned_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        let mut ft = FieldType::new(TypeShort);
        ft.set_flag(UnsignedFlag);
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: ft,
            state: StatePublic,
            ..Default::default()
        })
    }

    fn json_col(id: i64, offset: isize, name: &str) -> Arc<ColumnInfo> {
        Arc::new(ColumnInfo {
            id,
            name: CIStr::new(name),
            offset,
            field_type: FieldType::new(TypeJSON),
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

    fn encode_v1_record(
        table_id: i64,
        row_id: i64,
        col_ids: &[i64],
        datums: &[Datum],
    ) -> (Vec<u8>, Vec<u8>) {
        let mut enc = RowEncoder::default();
        enc.enable = false;
        let buf = tc::encode_row(Some(&utc()), datums, col_ids, None, Some(&mut enc)).unwrap();
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
    fn decode_all_eight_mysql_privilege_tables_v1_v2_and_common_handle() {
        let mut mp = MySQLPrivilege::new();

        // 1. mysql.user (bootstrap-style columns, mixing row format v1 and v2, plus plugin,
        // password_expired, password_lifetime, and User_attributes failed-login policy).
        let user_tbl = TableInfo {
            id: 5,
            name: CIStr::new("user"),
            columns: Some(vec![
                char_col(1, 0, "Host"),
                char_col(2, 1, "User"),
                varchar_col(3, 2, "authentication_string"),
                enum_yn_col(4, 3, "Select_priv"),
                enum_yn_col(5, 4, "Super_priv"),
                enum_yn_col(6, 5, "Account_locked"),
                char_col(7, 6, "Plugin"),
                enum_yn_col(8, 7, "password_expired"),
                smallint_unsigned_col(9, 8, "password_lifetime"),
                json_col(10, 9, "User_attributes"),
                timestamp_col(11, 10, "Password_last_changed"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let yn = &[b"N".to_vec(), b"Y".to_vec()];
        let e_y = new_mysql_enum_datum(parse_enum_name(yn, b"Y", "").unwrap());
        let e_n = new_mysql_enum_datum(parse_enum_name(yn, b"N", "").unwrap());
        let ts_2026_utc = new_time_datum(new_time(
            from_date_checked(2026, 11, 1, 12, 30, 45, 0).0,
            TypeTimestamp,
            0,
        ));
        let ts_zero = new_time_datum(new_time(ZeroCoreTime, TypeTimestamp, 0));
        let mut lock_policy_json = Datum::default();
        lock_policy_json.set_mysql_json(
            parse_binary_json_from_string(
                br#"{"Password_locking":{"failed_login_attempts":3,"password_lock_time_days":2}}"#,
            )
            .unwrap(),
        );

        let user_rows = vec![
            // v1-encoded root row (empty Plugin normalizes to mysql_native_password).
            encode_v1_record(
                5,
                1,
                &[1, 2, 3, 4, 5, 6, 7, 8],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("root".to_owned()),
                    new_string_datum("*23AE809DDACAF96AF0FD78ED04B6A265E05AA257".to_owned()),
                    e_y.clone(),
                    e_y.clone(),
                    e_n.clone(),
                    new_string_datum(String::new()),
                    e_n.clone(),
                ],
            ),
            // v2-encoded normal account with a concrete UTC Password_last_changed timestamp.
            encode_v2_record(
                5,
                2,
                &[1, 2, 3, 4, 5, 6, 7, 11],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_app".to_owned()),
                    new_string_datum(String::new()),
                    e_n.clone(),
                    e_n.clone(),
                    e_n.clone(),
                    new_string_datum("mysql_native_password".to_owned()),
                    ts_2026_utc,
                ],
            ),
            // v2-encoded locked role row with zero Password_last_changed (decodes to None).
            encode_v2_record(
                5,
                3,
                &[1, 2, 3, 4, 5, 6, 11],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("r_reader".to_owned()),
                    new_string_datum(String::new()),
                    e_n.clone(),
                    e_n.clone(),
                    e_y.clone(),
                    ts_zero,
                ],
            ),
            // v2-encoded non-native plugin (`tidb_auth_token`) with empty authentication_string.
            encode_v2_record(
                5,
                4,
                &[1, 2, 3, 4, 5, 6, 7],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_token".to_owned()),
                    new_string_datum(String::new()),
                    e_y.clone(),
                    e_n.clone(),
                    e_n.clone(),
                    new_string_datum("tidb_auth_token".to_owned()),
                ],
            ),
            // v2-encoded expired password + password_lifetime + Password_locking JSON.
            encode_v2_record(
                5,
                5,
                &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_restricted".to_owned()),
                    new_string_datum(String::new()),
                    e_y.clone(),
                    e_n.clone(),
                    e_n.clone(),
                    new_string_datum("mysql_native_password".to_owned()),
                    e_y.clone(),
                    new_uint_datum(30),
                    lock_policy_json,
                ],
            ),
        ];
        decode_mysql_user(&user_tbl, &user_rows, &mut mp).unwrap();

        // 1b. mysql.global_priv (REQUIRE SSL, NONE, and broken JSON).
        let gp_tbl = TableInfo {
            id: 12,
            name: CIStr::new("global_priv"),
            columns: Some(vec![
                char_col(1, 0, "Host"),
                char_col(2, 1, "User"),
                varchar_col(3, 2, "Priv"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let gp_rows = vec![
            encode_v2_record(
                12,
                1,
                &[1, 2, 3],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_ssl".to_owned()),
                    new_string_datum(r#"{"ssl_type":1}"#.to_owned()),
                ],
            ),
            encode_v2_record(
                12,
                2,
                &[1, 2, 3],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("u_broken_gp".to_owned()),
                    new_string_datum("{not-json".to_owned()),
                ],
            ),
            encode_v2_record(
                12,
                3,
                &[1, 2, 3],
                &[
                    new_string_datum("%".to_owned()),
                    new_string_datum("root".to_owned()),
                    new_string_datum(r#"{"ssl_type":0}"#.to_owned()),
                ],
            ),
        ];
        decode_mysql_global_priv(&gp_tbl, &gp_rows, &mut mp).unwrap();

        // 2. mysql.db (clustered common-handle CHAR PK on (Host, DB, User)).
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
                char_col(1, 0, "Host"),
                char_col(2, 1, "DB"),
                char_col(3, 2, "User"),
                char_col(4, 3, "Table_name"),
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
                char_col(1, 0, "Host"),
                char_col(2, 1, "DB"),
                char_col(3, 2, "User"),
                char_col(4, 3, "Table_name"),
                char_col(5, 4, "Column_name"),
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
                char_col(1, 0, "Host"),
                char_col(2, 1, "User"),
                char_col(3, 2, "Priv"),
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
                char_col(1, 0, "FROM_HOST"),
                char_col(2, 1, "FROM_USER"),
                char_col(3, 2, "TO_HOST"),
                char_col(4, 3, "TO_USER"),
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
                char_col(1, 0, "HOST"),
                char_col(2, 1, "USER"),
                char_col(3, 2, "DEFAULT_ROLE_HOST"),
                char_col(4, 3, "DEFAULT_ROLE_USER"),
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

        // 8. mysql.global_variables (`default_password_lifetime`).
        let gv_tbl = TableInfo {
            id: 13,
            name: CIStr::new("global_variables"),
            columns: Some(vec![
                varchar_col(1, 0, "VARIABLE_NAME"),
                varchar_col(2, 1, "VARIABLE_VALUE"),
            ]),
            state: StatePublic,
            ..Default::default()
        };
        let gv_rows = vec![encode_v2_record(
            13,
            1,
            &[1, 2],
            &[
                new_string_datum("default_password_lifetime".to_owned()),
                new_string_datum("90".to_owned()),
            ],
        )];
        decode_mysql_global_variables(&gv_tbl, &gv_rows, &mut mp).unwrap();

        let handle = Arc::new(Handle::new(mp));
        let snap = handle.get();

        // Verify v1-encoded root row, auth_plugin normalization, and privileges.
        let root_rec = snap.match_user("root", "127.0.0.1").unwrap();
        assert_eq!(
            root_rec.auth_string,
            "*23AE809DDACAF96AF0FD78ED04B6A265E05AA257"
        );
        assert_eq!(root_rec.auth_plugin, "mysql_native_password");
        assert!(!root_rec.account_locked);
        assert!(!root_rec.password_expired);
        assert_eq!(root_rec.password_lifetime, None);
        assert!(!root_rec.has_failed_login_policy);
        assert_eq!(
            root_rec.privileges.0 & (SelectPriv.0 | SuperPriv.0),
            SelectPriv.0 | SuperPriv.0
        );

        let token_rec = snap.match_user("u_token", "127.0.0.1").unwrap();
        assert_eq!(token_rec.auth_plugin, "tidb_auth_token");

        let restricted_rec = snap.match_user("u_restricted", "127.0.0.1").unwrap();
        assert!(restricted_rec.password_expired);
        assert_eq!(restricted_rec.password_lifetime, Some(30));
        assert!(!restricted_rec.password_lifetime_invalid);
        assert_eq!(restricted_rec.password_last_changed, None);
        assert!(restricted_rec.has_failed_login_policy);
        assert_eq!(snap.default_password_lifetime(), Some(90));

        // Verify UTC `Password_last_changed` decoding (concrete timestamp vs zero timestamp).
        let app_rec = snap.match_user("u_app", "127.0.0.1").unwrap();
        let (expected_gt, err) = new_time(
            from_date_checked(2026, 11, 1, 12, 30, 45, 0).0,
            TypeTimestamp,
            0,
        )
        .go_time(&utc());
        err.unwrap();
        assert_eq!(app_rec.password_last_changed, Some(expected_gt.unix()));
        let role_rec = snap.match_user("r_reader", "127.0.0.1").unwrap();
        assert_eq!(role_rec.password_last_changed, None);

        // Verify unparseable `default_password_lifetime` fails closed (`None`).
        let bad_gv_rows = vec![encode_v2_record(
            13,
            2,
            &[1, 2],
            &[
                new_string_datum("default_password_lifetime".to_owned()),
                new_string_datum("not-a-number".to_owned()),
            ],
        )];
        let mut mp_bad_gv = MySQLPrivilege::new();
        decode_mysql_global_variables(&gv_tbl, &bad_gv_rows, &mut mp_bad_gv).unwrap();
        assert_eq!(mp_bad_gv.default_password_lifetime(), None);

        // Verify unrelated mysql.* table names are skipped by `peek_mysql_table_lower_name`.
        assert_eq!(
            peek_mysql_table_lower_name(br#"{"name":{"O":"stats_meta","L":"stats_meta"}}"#)
                .as_deref(),
            Some("stats_meta")
        );
        assert!(!LOADED_MYSQL_TABLES.contains(&"stats_meta"));

        // Verify mysql.global_priv SSL & broken JSON decoding.
        let gp_ssl = snap.match_global_priv("u_ssl", "127.0.0.1").unwrap();
        assert!(gp_ssl.require_ssl && !gp_ssl.broken);
        let gp_broken = snap.match_global_priv("u_broken_gp", "127.0.0.1").unwrap();
        assert!(gp_broken.broken && gp_broken.require_ssl);
        let gp_root = snap.match_global_priv("root", "127.0.0.1").unwrap();
        assert!(!gp_root.require_ssl && !gp_root.broken);

        // Verify common-handle mysql.db decoding.
        let u_app = UserPrivileges::new("u_app", "%", Arc::clone(&handle));
        let default_roles = snap.get_default_roles("u_app", "%");
        assert_eq!(default_roles.len(), 1);
        assert_eq!(default_roles[0].username, "r_reader");

        assert!(u_app.request_verification(&[], "appdb", "x", "", InsertPriv));
        assert!(!u_app.request_verification(&[], "appdb", "x", "*", SelectPriv));
        assert!(u_app.db_is_visible(&[], "appdb"));

        // Verify mysql.columns_priv, tables_priv via role edge, and global_grants.
        assert!(u_app.request_verification(&[], "test", "t", "*", SelectPriv));
        assert!(u_app.request_verification(&[], "test", "t", "a", SelectPriv));
        assert!(!u_app.request_verification(&[], "test", "t", "b", SelectPriv));
        assert!(!u_app.request_verification(&[], "test", "t_role", "*", SelectPriv));
        assert!(u_app.request_verification(&default_roles, "test", "t_role", "*", SelectPriv));
        assert!(u_app.request_verification(&default_roles, "test", "t_role", "", UpdatePriv));
        assert!(u_app.request_dynamic_verification(&[], "BACKUP_ADMIN", true));

        // Verify missing required column fails hard instead of silently defaulting to "".
        let bad_user_tbl = TableInfo {
            id: 99,
            name: CIStr::new("user"),
            columns: Some(vec![char_col(1, 0, "Host")]),
            state: StatePublic,
            ..Default::default()
        };
        let mut mp_bad = MySQLPrivilege::new();
        assert!(decode_mysql_user(&bad_user_tbl, &[], &mut mp_bad).is_err());
    }
}
