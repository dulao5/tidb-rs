//! Unit tests of the session / domain functions of D06.7 over an in-memory store with
//! optimistic conflict detection (the TiKV behaviour the writer relies on: a write conflict
//! fails the commit, a presume-not-exists key that exists fails it too).

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use tidb_base::Error;
use tidb_model::TableInfo;
use tidb_systable::autoid::IdAccessor;
use tidb_systable::memdb::{MemBuffer, Snapshot};
use tidb_systable::replace::{RowEncodingConfig, replace_row};
use tidb_systable::session_io::*;

fn tables() -> HashMap<String, Arc<TableInfo>> {
    let path = format!(
        "{}/tests/vectors/v8.5.8/arm64/write_systable.jsonl.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut gz = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
    let mut text = String::new();
    gz.read_to_string(&mut text).unwrap();
    text.lines()
        .skip(1)
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|r| r["op"] == "tableinfo")
        .map(|r| {
            let mut t = TableInfo::default();
            t.unmarshal_json(r["out"]["info"].to_string().as_bytes())
                .unwrap();
            (r["args"][0].as_str().unwrap().to_owned(), Arc::new(t))
        })
        .collect()
}

/// A concurrent writer run between a transaction's start and its commit.
type Interleave = Mutex<Option<Box<dyn FnOnce(&Store) + Send>>>;
/// Committed values with the version that wrote them.
type Data = Mutex<BTreeMap<Vec<u8>, (u64, Vec<u8>)>>;

/// Committed data with a version per key (for write-conflict detection).
#[derive(Default)]
struct Store {
    data: Data,
    version: Mutex<u64>,
    meta: Mutex<HashMap<i64, i64>>,
    tables: HashMap<String, Arc<TableInfo>>,
    /// Runs once between a transaction's start and its commit (a concurrent writer).
    interleave: Interleave,
    commits: AtomicUsize,
}

impl Store {
    fn apply(&self, mb: &MemBuffer) {
        let mut v = self.version.lock();
        *v += 1;
        let mut d = self.data.lock();
        for (k, e) in mb.entries() {
            if e.value.is_empty() {
                d.remove(k);
            } else {
                d.insert(k.clone(), (*v, e.value.clone()));
            }
        }
    }
}

struct Txn {
    store: Arc<Store>,
    start: u64,
    snap: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Snapshot for Txn {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.snap.get(key).cloned())
    }
}

impl SysTableTxn for Txn {
    fn table_info(&mut self, name: &str) -> Result<Arc<TableInfo>, Error> {
        Ok(Arc::clone(&self.store.tables[name]))
    }
    fn commit(self: Box<Self>, mb: &MemBuffer, _: &[Arc<TableInfo>]) -> Result<(), CommitError> {
        let f = self.store.interleave.lock().take();
        if let Some(f) = f {
            f(&self.store);
        }
        let d = self.store.data.lock();
        for (k, e) in mb.entries() {
            if let Some((ver, _)) = d.get(k)
                && *ver > self.start
            {
                return Err(CommitError::Retryable(tidb_base::errors::new(
                    "write conflict",
                )));
            }
            if e.flags.presume_not_exists && d.contains_key(k) {
                return Err(CommitError::Fatal(tidb_base::errors::new("key exists")));
            }
        }
        drop(d);
        self.store.apply(mb);
        self.store.commits.fetch_add(1, SeqCst);
        Ok(())
    }
}

struct StoreRef(Arc<Store>);

impl SysTableStore for StoreRef {
    fn begin(&self) -> Result<Box<dyn SysTableTxn>, Error> {
        let start = *self.0.version.lock();
        let snap = self
            .0
            .data
            .lock()
            .iter()
            .map(|(k, (_, v))| (k.clone(), v.clone()))
            .collect();
        Ok(Box::new(Txn {
            store: Arc::clone(&self.0),
            start,
            snap,
        }))
    }
    fn run_meta_txn(
        &self,
        _db: i64,
        table_id: i64,
        f: &mut dyn FnMut(&mut dyn IdAccessor) -> Result<(), Error>,
    ) -> Result<(), Error> {
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
        let mut m = self.0.meta.lock();
        let c = m.entry(table_id).or_insert(0);
        let mut tmp = *c;
        f(&mut Acc(&mut tmp))?;
        *c = tmp;
        Ok(())
    }
}

#[derive(Default)]
struct Notes {
    puts: Mutex<Vec<String>>,
    pd: Mutex<Vec<(String, String)>>,
    warns: Mutex<Vec<String>>,
}

impl Notifier for Notes {
    fn etcd_put(&self, key: &str) -> Option<Result<(), Error>> {
        self.puts.lock().push(key.to_owned());
        Some(Ok(()))
    }
    fn global_config_put(&self, name: &str, value: &str) -> Result<(), Error> {
        self.pd.lock().push((name.to_owned(), value.to_owned()));
        Ok(())
    }
    fn warn(&self, msg: &str, _: &Error) {
        self.warns.lock().push(msg.to_owned());
    }
}

/// A store holding mysql.tidb's tidb_server_version row.
fn store(version: &str) -> Arc<Store> {
    let s = Arc::new(Store {
        tables: tables(),
        ..Store::default()
    });
    let mut mb = MemBuffer::new();
    replace_row(
        &s.tables["tidb"],
        &[
            Some("tidb_server_version"),
            Some(version),
            Some("Bootstrap version. Do not delete."),
        ],
        &BTreeMap::new(),
        &mut mb,
        &mut || Ok(1),
        &RowEncodingConfig::default(),
    )
    .unwrap();
    s.apply(&mb);
    s.meta.lock().insert(s.tables["tidb"].id, 1);
    s
}

#[test]
fn write_read_back_and_notify() {
    let s = store("232");
    let w = SysTables::new(Arc::new(StoreRef(Arc::clone(&s))));
    let notes = Notes::default();
    let rebuilds = AtomicUsize::new(0);
    let mut rebuild = || {
        rebuilds.fetch_add(1, SeqCst);
        Ok(())
    };
    w.replace_global_variables_table_value("max_execution_time", "100", true, &notes, &mut rebuild)
        .unwrap();
    assert_eq!(
        w.get_table_value("global_variables", "max_execution_time")
            .unwrap(),
        "100"
    );
    // PAD SPACE: trailing spaces of the name do not matter.
    assert_eq!(
        w.get_table_value("global_variables", "max_execution_time  ")
            .unwrap(),
        "100"
    );
    w.replace_global_variables_table_value(
        "max_execution_time",
        "200",
        false,
        &notes,
        &mut rebuild,
    )
    .unwrap();
    assert_eq!(
        w.get_table_value("global_variables", "max_execution_time")
            .unwrap(),
        "200"
    );
    let commits = s.commits.load(SeqCst);
    // Unchanged: nothing to commit, but Go still notifies.
    w.replace_global_variables_table_value(
        "max_execution_time",
        "200",
        false,
        &notes,
        &mut rebuild,
    )
    .unwrap();
    assert_eq!(s.commits.load(SeqCst), commits);
    assert_eq!(notes.puts.lock().len(), 3);
    assert_eq!(rebuilds.load(SeqCst), 1);
    assert!(w.get_table_value("global_variables", "nope").is_err());
    // mysql.tidb (tidb_gc_*): three columns.
    w.set_tidb_table_value("tikv_gc_life_time", "20m0s", "c")
        .unwrap();
    assert_eq!(
        w.get_tidb_table_value("tikv_gc_life_time").unwrap(),
        "20m0s"
    );
    notify_global_config_change(&notes, "enable_resource_control", "true");
    assert_eq!(notes.pd.lock().len(), 1);
}

#[test]
fn concurrent_writer_last_commit_wins_after_retry() {
    let s = store("232");
    let w = Arc::new(SysTables::new(Arc::new(StoreRef(Arc::clone(&s)))));
    let notes = Notes::default();
    w.replace_global_variables_table_value("v1", "a", false, &notes, &mut || Ok(()))
        .unwrap();
    // Another instance changes the row between our start and our commit: our commit hits a
    // write conflict, the statement is retried with a new snapshot and wins.
    let w2 = SysTables::new(Arc::new(StoreRef(Arc::clone(&s))));
    *s.interleave.lock() = Some(Box::new(move |_| {
        w2.replace_global_variables_table_value(
            "v1",
            "b",
            false,
            &Notes::default(),
            &mut || Ok(()),
        )
        .unwrap();
    }));
    w.replace_global_variables_table_value("v1", "c", false, &notes, &mut || Ok(()))
        .unwrap();
    assert_eq!(w.get_table_value("global_variables", "v1").unwrap(), "c");
    // Retry budget exhausted: the conflict error comes back.
    let mut w3 = SysTables::new(Arc::new(StoreRef(Arc::clone(&s))));
    w3.retry_limit = 0;
    let s2 = Arc::clone(&s);
    *s.interleave.lock() = Some(Box::new(move |_| {
        SysTables::new(Arc::new(StoreRef(Arc::clone(&s2))))
            .replace_global_variables_table_value("v1", "d", false, &Notes::default(), &mut || {
                Ok(())
            })
            .unwrap();
    }));
    assert!(
        w3.replace_global_variables_table_value("v1", "e", false, &notes, &mut || Ok(()))
            .is_err()
    );
    assert_eq!(w.get_table_value("global_variables", "v1").unwrap(), "d");
}

#[test]
fn bootstrap_version_mismatch_is_refused() {
    let s = store("231");
    let w = SysTables::new(Arc::new(StoreRef(Arc::clone(&s))));
    let err = w
        .replace_global_variables_table_value("v", "1", false, &Notes::default(), &mut || Ok(()))
        .unwrap_err();
    assert!(String::from_utf8_lossy(&err.error()).contains("bootstrap version 231"));
}

#[test]
fn load_loop_rebuilds_on_events_and_rewatches() {
    let notes = Notes::default();
    let mut events = vec![
        LoopEvent::Tick,
        LoopEvent::Watch,
        LoopEvent::WatchClosed,
        LoopEvent::Watch,
        LoopEvent::Exit,
    ]
    .into_iter();
    let rebuilds = AtomicUsize::new(0);
    let rewatches = AtomicUsize::new(0);
    load_sys_var_cache_loop(
        &mut || {
            rebuilds.fetch_add(1, SeqCst);
            Ok(())
        },
        &mut || events.next().unwrap(),
        &mut || {
            rewatches.fetch_add(1, SeqCst);
        },
        &mut |_: Duration| {},
        &notes,
    )
    .unwrap();
    // initial + tick + watch + (closed: rebuild) + watch
    assert_eq!(rebuilds.load(SeqCst), 5);
    assert_eq!(rewatches.load(SeqCst), 1);
}
