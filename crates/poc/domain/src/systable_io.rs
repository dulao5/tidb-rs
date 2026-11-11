//! The I/O of the SET GLOBAL path (D05.2 spec §6.2, block D06.7): tidb-systable's
//! [`SysTableStore`] and [`Notifier`] over tikv-client and etcd.
//!
//! The traits are synchronous (the writer is pure logic); these implementations block on the
//! given tokio runtime handle, so they must be called from a thread that is not a runtime
//! worker (a blocking-pool thread or a plain thread). `SET GLOBAL` is rare: no async path.
//!
//! Not yet done (spec §6.2 step 1 / 5 / 8): the `internal_sysvar` request source (no
//! request-source option in the client-rust fork), assertions (dropped: TiKV accepts a
//! mutation without assertion under any `tidb_txn_assertion_level`), and the PD
//! GlobalConfig write (no GlobalConfig API in the fork: [`EtcdNotifier::global_config_put`]
//! returns an error, which the writer logs as Go logs a failed sync).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use poc_kv::{KeyRange, KvClient, is_write_conflict};
use tidb_base::Error;
use tidb_meta::keys;
use tidb_model::TableInfo;
use tidb_systable::autoid::IdAccessor;
use tidb_systable::memdb::{MemBuffer, Snapshot};
use tidb_systable::session_io::{CommitError, MYSQL_DB_ID, Notifier, SysTableStore, SysTableTxn};
use tokio::runtime::Handle;

fn err(e: impl std::fmt::Display) -> Error {
    tidb_base::errors::new(e.to_string())
}

/// `kv.RunInNewTxn`'s retry count for the meta transaction (Go: `maxRetryCnt` 100, with
/// backoff; one meta row is contended only by allocators fetching a new range).
const META_TXN_RETRIES: usize = 100;

/// tidb-systable's store over TiKV.
pub struct TikvSysTables {
    kv: KvClient,
    rt: Handle,
    /// Table ID of mysql.<name> (stable: system tables are never re-created).
    ids: Mutex<HashMap<String, i64>>,
}

impl TikvSysTables {
    pub fn new(kv: KvClient, rt: Handle) -> TikvSysTables {
        TikvSysTables {
            kv,
            rt,
            ids: Mutex::new(HashMap::new()),
        }
    }

    /// The meta key of mysql.<name>'s TableInfo, looking the ID up once.
    fn table_meta_key(&self, name: &str) -> Result<Vec<u8>, Error> {
        if let Some(id) = self.ids.lock().get(name) {
            return Ok(keys::hash_data_key(
                &keys::db_key(MYSQL_DB_ID),
                &keys::table_key(*id),
            ));
        }
        let prefix = keys::hash_data_prefix(&keys::db_key(MYSQL_DB_ID));
        let range = KeyRange::new(prefix.clone(), keys::prefix_next(&prefix));
        let pairs = self.rt.block_on(async {
            let ts = self.kv.current_ts().await?;
            self.kv.snapshot(ts).scan_all(range).await
        });
        let pairs = pairs.map_err(err)?;
        let mut ids = self.ids.lock();
        for (k, v) in pairs {
            let Some(field) = keys::decode_hash_field(&k, &prefix) else {
                continue;
            };
            let Some(id) = keys::parse_table_key(&field) else {
                continue;
            };
            let mut t = TableInfo::default();
            if t.unmarshal_json(&v).is_ok() {
                ids.insert(t.name.l.clone(), id);
            }
        }
        match ids.get(name) {
            Some(id) => Ok(keys::hash_data_key(
                &keys::db_key(MYSQL_DB_ID),
                &keys::table_key(*id),
            )),
            None => Err(err(format!("mysql.{name} not found in meta"))),
        }
    }
}

/// One optimistic transaction.
pub struct TikvTxn {
    store: Arc<TikvSysTables>,
    txn: Mutex<tikv_client::Transaction>,
    /// The TableInfo JSON each table had at start (compared again before the commit).
    seen: HashMap<String, Vec<u8>>,
}

impl Snapshot for TikvTxn {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let mut t = self.txn.lock();
        self.store.rt.block_on(t.get(key.to_vec())).map_err(err)
    }
}

impl SysTableTxn for TikvTxn {
    fn table_info(&mut self, name: &str) -> Result<Arc<TableInfo>, Error> {
        let key = self.store.table_meta_key(name)?;
        let Some(raw) = self.get(&key)? else {
            return Err(err(format!("mysql.{name}: no TableInfo at the start ts")));
        };
        let mut t = TableInfo::default();
        t.unmarshal_json(&raw)?;
        self.seen.insert(name.to_owned(), raw);
        Ok(Arc::new(t))
    }

    fn commit(
        self: Box<Self>,
        mb: &MemBuffer,
        tables: &[Arc<TableInfo>],
    ) -> Result<(), CommitError> {
        let TikvTxn { store, txn, seen } = *self;
        let mut txn = txn.into_inner();
        let rt = store.rt.clone();
        rt.block_on(async {
            // The TableInfo must not have changed since the start (spec §6.4: no schema
            // validator / MDL): read it again at a fresh ts.
            let ts = store
                .kv
                .current_ts()
                .await
                .map_err(|e| CommitError::Fatal(err(e)))?;
            let mut snap = store.kv.snapshot(ts);
            for t in tables {
                let key = store
                    .table_meta_key(&t.name.l)
                    .map_err(CommitError::Fatal)?;
                let now = snap
                    .get(key)
                    .await
                    .map_err(|e| CommitError::Fatal(err(e)))?;
                if now.as_deref() != seen.get(&t.name.l).map(Vec::as_slice) {
                    let _ = txn.rollback().await;
                    return Err(CommitError::Retryable(err(format!(
                        "mysql.{} changed during SET GLOBAL",
                        t.name.l
                    ))));
                }
            }
            for (k, e) in mb.entries() {
                let r = if e.value.is_empty() {
                    txn.delete(k.clone()).await
                } else if e.flags.presume_not_exists {
                    // client-go sends a presume-not-exists key as Op_Insert.
                    txn.insert(k.clone(), e.value.clone()).await
                } else {
                    txn.put(k.clone(), e.value.clone()).await
                };
                r.map_err(|e| CommitError::Fatal(err(e)))?;
            }
            match txn.commit().await {
                Ok(_) => Ok(()),
                Err(e) if is_write_conflict(&e) => Err(CommitError::Retryable(err(e))),
                Err(e) => Err(CommitError::Fatal(err(e))),
            }
        })
    }
}

/// [`SysTableStore`] needs the `Arc` to hand it to each transaction.
pub struct TikvStore(pub Arc<TikvSysTables>);

impl SysTableStore for TikvStore {
    fn begin(&self) -> Result<Box<dyn SysTableTxn>, Error> {
        let txn = self
            .0
            .rt
            .block_on(self.0.kv.begin_optimistic())
            .map_err(err)?;
        Ok(Box::new(TikvTxn {
            store: self.0.clone(),
            txn: Mutex::new(txn),
            seen: HashMap::new(),
        }))
    }

    fn run_meta_txn(
        &self,
        db_id: i64,
        table_id: i64,
        f: &mut dyn FnMut(&mut dyn IdAccessor) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // meta.autoTableIDKey: field "TID:<id>" of hash "DB:<id>"; the value is the decimal
        // counter (structure.HInc / HGetInt64).
        let key = keys::hash_data_key(&keys::db_key(db_id), format!("TID:{table_id}").as_bytes());
        let s = &self.0;
        let mut last = None;
        for _ in 0..META_TXN_RETRIES {
            let r: Result<Result<(), Error>, tikv_client::Error> = s.rt.block_on(async {
                let mut txn = s.kv.begin_optimistic().await?;
                let old = txn.get(key.clone()).await?;
                let mut val = match &old {
                    Some(v) => match std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()) {
                        Some(n) => n,
                        None => return Ok(Err(err("bad row-ID counter"))),
                    },
                    None => 0,
                };
                struct Acc<'a>(&'a mut i64);
                impl IdAccessor for Acc<'_> {
                    fn get(&mut self) -> Result<i64, Error> {
                        Ok(*self.0)
                    }
                    fn inc(&mut self, step: i64) -> Result<i64, Error> {
                        *self.0 += step;
                        Ok(*self.0)
                    }
                }
                if let Err(e) = f(&mut Acc(&mut val)) {
                    let _ = txn.rollback().await;
                    return Ok(Err(e));
                }
                let new = val.to_string().into_bytes();
                // structure.updateHash: no write when the value is unchanged.
                if old.as_deref() != Some(new.as_slice()) {
                    txn.put(key.clone(), new).await?;
                }
                txn.commit().await?;
                Ok(Ok(()))
            });
            match r {
                Ok(inner) => return inner,
                Err(e) if is_write_conflict(&e) => last = Some(e),
                Err(e) => return Err(err(e)),
            }
        }
        Err(err(last.map(|e| e.to_string()).unwrap_or_default()))
    }
}

/// [`Notifier`] over etcd (PD's embedded etcd).
pub struct EtcdNotifier {
    pub etcd: Option<etcd_client::Client>,
    pub rt: Handle,
}

impl Notifier for EtcdNotifier {
    fn etcd_put(&self, key: &str) -> Option<Result<(), Error>> {
        let mut c = self.etcd.clone()?;
        Some(
            self.rt
                .block_on(c.put(key, "", None))
                .map(|_| ())
                .map_err(err),
        )
    }
    fn global_config_put(&self, name: &str, _value: &str) -> Result<(), Error> {
        Err(err(format!(
            "PD GlobalConfig is not supported by tidb-rs yet ({name})"
        )))
    }
    fn warn(&self, msg: &str, e: &Error) {
        tracing::warn!("{msg}: {}", String::from_utf8_lossy(&e.error()));
    }
}
