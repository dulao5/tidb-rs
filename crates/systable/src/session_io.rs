//! go-file: pkg/session/session.go
//! go-file: pkg/domain/domain.go
//!
//! The session and domain functions of block D06.7 that read and write the system tables
//! and notify the other instances, over the caller's I/O ([`SysTableStore`], [`Notifier`]):
//! the TiKV transactions, etcd and PD clients live in the legacy `poc-domain` until D06.5 /
//! D07.1 (D05.2 spec §6.2).
//!
//! Go runs `REPLACE` / `SELECT` through the restricted SQL executor; here the writes are
//! [`replace::replace_row`] and the reads a unique-index point read, inside an optimistic
//! transaction that is retried as a whole on a retryable commit error, like Go's autocommit
//! retry (`tidb_retry_limit`), each attempt with a new start ts (spec §6.2 step 6).

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tidb_base::Error;
use tidb_base::errors;
use tidb_base::gostd::time::utc;
use tidb_model::TableInfo;
use tidb_tablecodec::tablecodec as tc;
use tidb_types::datum::new_collation_string_datum;

use crate::autoid::{IdAccessor, RowIdAllocator};
use crate::memdb::{MemBuffer, Snapshot};
use crate::replace::{self, RowEncodingConfig};

/// `mysql.SystemDB`'s ID (bootstrap creates it first).
pub const MYSQL_DB_ID: i64 = 1;
/// `currentBootstrapVersion` of v8.5.8 (`version232`): the system-table layout this writer
/// was checked against (spec §6.2 step 0).
pub const BOOTSTRAP_VERSION: i64 = 232;
/// `sysVarCacheKey`: the etcd key whose PUT tells every instance to rebuild its sysvar cache.
pub const SYS_VAR_CACHE_KEY: &str = "/tidb/sysvars";

/// Whether a failed commit may be retried with a new transaction (write conflict, or the
/// table changed between start and commit).
#[derive(Debug)]
pub enum CommitError {
    /// Retry the whole statement (Go: `isOptimisticTxnRetryable`).
    Retryable(Error),
    /// Give up.
    Fatal(Error),
}

/// One optimistic transaction of the caller (internal source `internal_sysvar`).
pub trait SysTableTxn: Snapshot {
    /// The TableInfo of `mysql.<name>` as of the transaction's start ts.
    // glue: the TableInfo the transaction sees (D06.2 meta read)
    fn table_info(&mut self, name: &str) -> Result<Arc<TableInfo>, Error>;
    /// Commits the buffer (2PC): a key flagged presume-not-exists is sent as an insert
    /// (TiKV checks it does not exist), a delete as a delete; before the prewrite the
    /// implementation re-reads the TableInfo of `tables` and fails with
    /// [`CommitError::Retryable`] if one changed (spec §6.4, no schema validator / MDL).
    // glue: 2PC of the buffer (kv.Transaction.Commit)
    fn commit(
        self: Box<Self>,
        mb: &MemBuffer,
        tables: &[Arc<TableInfo>],
    ) -> Result<(), CommitError>;
}

/// The caller's store: transactions on TiKV.
pub trait SysTableStore: Send + Sync {
    /// A new optimistic transaction with a start ts from the TSO.
    // glue: kv.Storage.Begin (optimistic, internal_sysvar)
    fn begin(&self) -> Result<Box<dyn SysTableTxn>, Error>;
    /// Runs f on the meta row-ID counter of (db_id, table_id) in a new transaction
    /// (`kv.RunInNewTxn(ctx, store, true, ...)`: retried on a write conflict).
    // glue: kv.RunInNewTxn over meta.AutoIDAccessor
    fn run_meta_txn(
        &self,
        db_id: i64,
        table_id: i64,
        f: &mut dyn FnMut(&mut dyn IdAccessor) -> Result<(), Error>,
    ) -> Result<(), Error>;
}

/// The caller's notification channels.
pub trait Notifier: Send + Sync {
    /// etcd `Put(key, "")` (`None` without an etcd client).
    // glue: etcd clientv3 KV.Put
    fn etcd_put(&self, key: &str) -> Option<Result<(), Error>>;
    /// PD GlobalConfig PUT of (name, value).
    // glue: PD GlobalConfig PUT (globalconfigsync)
    fn global_config_put(&self, name: &str, value: &str) -> Result<(), Error>;
    /// A warning Go logs.
    // glue: logutil.BgLogger().Warn
    fn warn(&self, msg: &str, err: &Error);
}

/// Writes and reads `mysql.global_variables` and `mysql.tidb` like the Go session does.
pub struct SysTables {
    store: Arc<dyn SysTableStore>,
    /// Row-ID allocators per table ID (the domain's, shared by all sessions).
    allocs: Mutex<Vec<RowIdAllocator>>,
    /// `tidb_retry_limit` as Go's internal session sees it: its default (10). The internal
    /// session never loads global variables (`createSessionWithDomainFunc` sets
    /// `CommonGlobalLoaded = true`, session.go:1214; tidb-rs #74).
    pub retry_limit: i64,
    /// The row encoding of Go's internal session: its defaults (row format 1, no checksum),
    /// not the cluster's global `tidb_row_format_version` / `tidb_enable_row_level_checksum`
    /// (same reason; tidb-rs #74).
    pub encoding: RowEncodingConfig,
}

impl SysTables {
    /// A writer over store, with the defaults of Go's internal session (retry limit 10, row
    /// format 1, no checksum), which never loads global variables (tidb-rs #74).
    // glue: the domain-level state of the writer (allocators, settings)
    pub fn new(store: Arc<dyn SysTableStore>) -> SysTables {
        SysTables {
            store,
            allocs: Mutex::new(Vec::new()),
            retry_limit: 10,
            encoding: RowEncodingConfig::default(),
        }
    }

    // glue: AllocHandle of the table's row-ID allocator
    fn alloc_handle(&self, tbl: &TableInfo) -> Result<i64, Error> {
        let mut allocs = self.allocs.lock();
        let now = Instant::now();
        let a = match allocs.iter_mut().position(|a| a.tb_id == tbl.id) {
            Some(i) => &mut allocs[i],
            None => {
                allocs.push(RowIdAllocator::new(
                    tbl.id,
                    tbl.auto_id_cache,
                    tbl.version,
                    now,
                ));
                allocs.last_mut().unwrap()
            }
        };
        let store = &self.store;
        let (_, max) = a.alloc(1, 1, 1, now, &mut |f| {
            store.run_meta_txn(MYSQL_DB_ID, tbl.id, f)
        })?;
        Ok(max)
    }

    /// The REPLACE of row into `mysql.<table>`, in an optimistic transaction retried as a
    /// whole on a retryable commit error (at most `retry_limit` retries).
    // glue: ExecRestrictedSQL("REPLACE INTO mysql.<table> ...") with the autocommit retry (session.go:812)
    fn replace(&self, table: &str, row: &[Option<&str>]) -> Result<(), Error> {
        let mut retries = 0;
        loop {
            let mut txn = self.store.begin()?;
            self.check_bootstrap_version(txn.as_mut())?;
            let tbl = txn.table_info(table)?;
            let mut mb = MemBuffer::new();
            let mut alloc = || self.alloc_handle(&tbl);
            replace::replace_row(&tbl, row, txn.as_ref(), &mut mb, &mut alloc, &self.encoding)?;
            if mb.is_empty() {
                // Unchanged row: a read-only optimistic transaction.
                return Ok(());
            }
            match txn.commit(&mb, &[tbl]) {
                Ok(()) => return Ok(()),
                Err(CommitError::Retryable(e)) if retries < self.retry_limit => {
                    let _ = e;
                    retries += 1;
                }
                Err(CommitError::Retryable(e)) | Err(CommitError::Fatal(e)) => return Err(e),
            }
        }
    }

    /// The bootstrap version must be the one the writer was checked against (spec §6.2
    /// step 0: during a rolling upgrade bootstrap runs DDL on mysql.*).
    // glue: spec §6.2 step 0 (no MDL / schema validator for the specialized writer)
    fn check_bootstrap_version(&self, txn: &mut dyn SysTableTxn) -> Result<(), Error> {
        let tidb = txn.table_info("tidb")?;
        let v = get_table_value_in(txn, &tidb, "tidb_server_version")?;
        if v.as_deref() != Some(&BOOTSTRAP_VERSION.to_string()) {
            return Err(errors::new(format!(
                "SET GLOBAL refused: cluster bootstrap version {} is not {BOOTSTRAP_VERSION} (tidb-rs v8.5.8); retry after the upgrade",
                v.as_deref().unwrap_or("<none>")
            )));
        }
        Ok(())
    }

    /// replaceGlobalVariablesTableValue executes restricted sql updates the variable value
    /// It will then notify the etcd channel that the value has changed.
    // go: session.replaceGlobalVariablesTableValue L2 (specialized writer instead of ExecRestrictedSQL)
    pub fn replace_global_variables_table_value(
        &self,
        var_name: &str,
        val: &str,
        update_local: bool,
        notifier: &dyn Notifier,
        rebuild: &mut dyn FnMut() -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.replace("global_variables", &[Some(var_name), Some(val)])?;
        notify_update_sys_var_cache(notifier, update_local, rebuild);
        Ok(())
    }

    /// SetTiDBTableValue implements GlobalVarAccessor.SetTiDBTableValue interface.
    // go: session.SetTiDBTableValue L2 (specialized writer instead of ExecRestrictedSQL)
    pub fn set_tidb_table_value(
        &self,
        name: &str,
        value: &str,
        comment: &str,
    ) -> Result<(), Error> {
        self.replace("tidb", &[Some(name), Some(value), Some(comment)])
    }

    /// getTableValue executes restricted sql and the result is one column.
    /// It returns a string value.
    // go: session.getTableValue L2 (unique-index point read instead of SELECT ... WHERE VARIABLE_NAME = ?)
    pub fn get_table_value(&self, tbl_name: &str, var_name: &str) -> Result<String, Error> {
        let mut txn = self.store.begin()?;
        let tbl = txn.table_info(tbl_name)?;
        match get_table_value_in(txn.as_mut(), &tbl, var_name)? {
            Some(v) => Ok(v),
            None => Err(tidb_base::dbterror::exeerrors::ErrResultIsEmpty.clone()),
        }
    }

    /// GetTiDBTableValue implements GlobalVarAccessor.GetTiDBTableValue interface.
    // go: session.GetTiDBTableValue L1
    pub fn get_tidb_table_value(&self, name: &str) -> Result<String, Error> {
        self.get_table_value("tidb", name)
    }
}

/// The VARIABLE_VALUE of the row whose VARIABLE_NAME is name (PAD SPACE: trailing spaces do
/// not matter), `None` if there is none; a NULL value reads as "".
// glue: point read through the unique index (the SELECT of getTableValue)
fn get_table_value_in(
    snap: &mut dyn SysTableTxn,
    tbl: &TableInfo,
    name: &str,
) -> Result<Option<String>, Error> {
    let cols = tbl.columns.as_deref().unwrap_or(&[]);
    let idx = tbl
        .indices
        .as_deref()
        .and_then(|i| i.first())
        .ok_or_else(|| errors::new(format!("mysql.{} has no primary key", tbl.name.l)))?;
    let name_col = &cols[0];
    let mut vals = vec![new_collation_string_datum(
        name.as_bytes().to_vec(),
        name_col.field_type.get_collate().to_owned(),
    )];
    let (key, _) = tc::gen_index_key(&utc(), tbl, idx, tbl.id, &mut vals, None, None)?;
    let Some(iv) = snap.get(&key)? else {
        return Ok(None);
    };
    let Some(h) = tc::decode_handle_in_index_value(&iv)? else {
        return Ok(None);
    };
    let Some(rv) = snap.get(&tc::encode_row_key_with_handle(tbl.id, &h))? else {
        return Ok(None);
    };
    let recs = [(tc::encode_row_key_with_handle(tbl.id, &h).to_vec(), rv)];
    let m = crate::sysvar_cache::decode_global_variables(tbl, &recs)?;
    Ok(m.into_values().next())
}

/// NotifyUpdateSysVarCache sends a notification to etcd that the sysvar cache has changed,
/// and (updateLocal) rebuilds the local cache.
// go: Domain.NotifyUpdateSysVarCache L2 (etcd and the rebuild are the caller's)
pub fn notify_update_sys_var_cache(
    notifier: &dyn Notifier,
    update_local: bool,
    rebuild: &mut dyn FnMut() -> Result<(), Error>,
) {
    if let Some(Err(e)) = notifier.etcd_put(SYS_VAR_CACHE_KEY) {
        notifier.warn("notify update sysvar cache failed", &e);
    }
    // update locally
    if update_local && let Err(e) = rebuild() {
        notifier.warn("rebuilding sysvar cache failed", &e);
    }
}

/// NotifyGlobalConfigChange notify global config syncer to store the global config into PD.
// go: Domain.NotifyGlobalConfigChange L2 (the PD client is the caller's; Go queues it to the syncer)
pub fn notify_global_config_change(notifier: &dyn Notifier, name: &str, value: &str) {
    if let Err(e) = notifier.global_config_put(name, value) {
        notifier.warn("global config sync failed", &e);
    }
}

/// What woke the sysvar cache loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopEvent {
    /// The domain is closing.
    Exit,
    /// An etcd watch event on [`SYS_VAR_CACHE_KEY`].
    Watch,
    /// The watch channel closed (or was compacted): watch again.
    WatchClosed,
    /// The 30 s timer.
    Tick,
}

/// The period of the sysvar cache reload when no notification comes.
pub const SYS_VAR_CACHE_PERIOD: Duration = Duration::from_secs(30);

/// LoadSysVarCacheLoop: rebuild once, then on every etcd watch event and every 30 s until
/// exit; a closed (or compacted) watch is re-created, with a growing sleep after 10 closes
/// in a row. `next` waits for the next event (watch or [`SYS_VAR_CACHE_PERIOD`]); `rewatch`
/// re-creates the watch; `sleep` sleeps.
// go: Domain.LoadSysVarCacheLoop L2 (events from the caller; rebuild on a compaction as on a close, spec §6.2)
pub fn load_sys_var_cache_loop(
    rebuild: &mut dyn FnMut() -> Result<(), Error>,
    next: &mut dyn FnMut() -> LoopEvent,
    rewatch: &mut dyn FnMut(),
    sleep: &mut dyn FnMut(Duration),
    notifier: &dyn Notifier,
) -> Result<(), Error> {
    rebuild()?;
    let mut count: u64 = 0;
    loop {
        match next() {
            LoopEvent::Exit => return Ok(()),
            LoopEvent::WatchClosed => {
                rewatch();
                count += 1;
                if count > 10 {
                    sleep(Duration::from_secs(count));
                }
                // spec §6.2: events may have been missed (compaction): rebuild now.
                if let Err(e) = rebuild() {
                    notifier.warn("LoadSysVarCacheLoop failed", &e);
                }
                continue;
            }
            LoopEvent::Watch | LoopEvent::Tick => {}
        }
        count = 0;
        if let Err(e) = rebuild() {
            notifier.warn("LoadSysVarCacheLoop failed", &e);
        }
    }
}
