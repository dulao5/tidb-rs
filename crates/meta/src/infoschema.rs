//! An immutable snapshot of all schemas, and a per-statement versioned cache of them.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use futures::FutureExt;
use futures::future::BoxFuture;
use futures::future::Shared;

use tidb_kv::KvClient;
use tidb_kv::KvSnapshot;

use crate::Result;
use crate::model::DbInfo;
use crate::model::SchemaState;
use crate::model::TableInfo;
use crate::reader::MetaReader;

/// Database and table names are compared case-insensitively, by their lower-case form (TiDB
/// supports only `lower_case_table_names = 2` semantics: names are stored as given and
/// looked up through `CIStr.L`).
fn lower(name: &str) -> String {
    name.to_lowercase()
}

/// One database and its public tables.
#[derive(Debug)]
struct SchemaEntry {
    info: DbInfo,
    tables: BTreeMap<String, Arc<TableInfo>>,
}

/// All public databases and tables as of one schema version (Go: `infoschema.InfoSchema`).
#[derive(Debug)]
pub struct InfoSchema {
    version: i64,
    schemas: BTreeMap<String, SchemaEntry>,
    tables_by_id: HashMap<i64, Arc<TableInfo>>,
}

impl InfoSchema {
    /// Build from loaded meta. Non-public databases and tables (being created or dropped)
    /// are left out, as they are invisible to reads in TiDB.
    pub fn new(version: i64, dbs: Vec<(DbInfo, Vec<TableInfo>)>) -> InfoSchema {
        let mut schemas = BTreeMap::new();
        let mut tables_by_id = HashMap::new();
        for (db, tables) in dbs {
            if db.state != SchemaState::Public {
                continue;
            }
            let mut by_name = BTreeMap::new();
            for table in tables {
                if table.state != SchemaState::Public {
                    continue;
                }
                let table = Arc::new(table);
                tables_by_id.insert(table.id, table.clone());
                by_name.insert(table.name.l.clone(), table);
            }
            schemas.insert(
                db.name.l.clone(),
                SchemaEntry {
                    info: db,
                    tables: by_name,
                },
            );
        }
        InfoSchema {
            version,
            schemas,
            tables_by_id,
        }
    }

    /// Load every database and table at the snapshot's timestamp. The version is the one
    /// whose changes are completely visible at that timestamp.
    pub async fn load(snapshot: &mut KvSnapshot) -> Result<InfoSchema> {
        let mut reader = MetaReader::new(snapshot);
        let version = reader.schema_version_with_non_empty_diff().await?;
        let mut dbs = Vec::new();
        for db in reader.list_databases().await? {
            if db.state != SchemaState::Public {
                continue;
            }
            let tables = reader.list_tables(db.id).await?;
            dbs.push((db, tables));
        }
        Ok(InfoSchema::new(version, dbs))
    }

    /// The schema version this snapshot corresponds to.
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Database by name (case-insensitive).
    pub fn schema_by_name(&self, db: &str) -> Option<&DbInfo> {
        self.schemas.get(&lower(db)).map(|s| &s.info)
    }

    /// Table by database and table name (case-insensitive).
    pub fn table_by_name(&self, db: &str, table: &str) -> Option<Arc<TableInfo>> {
        self.schemas
            .get(&lower(db))?
            .tables
            .get(&lower(table))
            .cloned()
    }

    pub fn table_by_id(&self, id: i64) -> Option<Arc<TableInfo>> {
        self.tables_by_id.get(&id).cloned()
    }

    /// All databases, ordered by lower-case name.
    pub fn schemas(&self) -> impl Iterator<Item = &DbInfo> {
        self.schemas.values().map(|s| &s.info)
    }

    /// Tables of a database ordered by lower-case name; empty if it does not exist.
    pub fn tables(&self, db: &str) -> Vec<Arc<TableInfo>> {
        self.schemas
            .get(&lower(db))
            .map(|s| s.tables.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// Number of schema versions kept by [`SchemaCache`] (Go keeps 16 by default).
const CACHE_CAPACITY: usize = 8;

#[derive(Clone)]
struct CacheEntry {
    schema: Arc<InfoSchema>,
    /// Smallest timestamp at which the diff of `schema.version` was seen to exist. From then
    /// on, a version key equal to `schema.version` means exactly this schema.
    diff_seen_at: Option<u64>,
}

/// Statistics of [`SchemaCache`] lookups (for tests and metrics).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub full_loads: u64,
    /// [`SchemaCache::check_at`] calls that confirmed the candidate schema.
    pub checks_passed: u64,
    /// [`SchemaCache::check_at`] calls that rejected it (the statement must re-plan).
    pub checks_failed: u64,
    /// Version-key reads issued by [`SchemaCache::check_at`] (several checks share one).
    pub version_reads: u64,
}

/// Result of [`SchemaCache::check_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaCheck {
    /// The candidate is exactly the schema visible at `start_ts`.
    Valid,
    /// It may not be. `raw` is the version key at `start_ts` if it was read at exactly
    /// `start_ts` (to be passed to [`SchemaCache::info_schema_with_raw`]).
    Stale { raw: Option<i64> },
}

/// The acceptance rule of [`SchemaCache::check_at`]: a candidate of version `candidate` is the
/// schema visible at `start_ts` if the version key read at some `read_ts >= start_ts` is
/// `raw == candidate` and the cache entry of `raw` proves that the key already was `raw`, with
/// its diff visible, at `diff_seen_at <= start_ts`. The version key only ever increases with
/// commit order (each DDL increments it in a transaction), so it is `raw` at every timestamp
/// in `[diff_seen_at, read_ts]`, in particular at `start_ts`, and the effective version there
/// is `raw`, not `raw - 1`. With `read_ts == start_ts` this is exactly the condition under
/// which [`SchemaCache::info_schema_at`] returns the cached entry after a single get.
fn check_accepts(entry: Option<&CacheEntry>, candidate: i64, raw: i64, start_ts: u64) -> bool {
    raw == candidate
        && entry.is_some_and(|e| {
            e.schema.version() == raw && e.diff_seen_at.is_some_and(|t| t <= start_ts)
        })
}

/// Versioned cache of [`InfoSchema`]s, consulted once per statement with its `start_ts`.
///
/// See `docs/adr/0003-schema-loading.md` for why this keeps the read-only node consistent.
pub struct SchemaCache {
    kv: KvClient,
    state: Mutex<CacheState>,
    /// Newest `start_ts` passed to [`SchemaCache::check_at`] (a TSO PD has issued).
    max_ts: AtomicU64,
    probes: Mutex<Probes>,
}

/// A version-key read shared by concurrent checks.
type Probe = Shared<BoxFuture<'static, std::result::Result<i64, Arc<crate::Error>>>>;

/// Version-key reads of [`SchemaCache::check_at`]: the one in flight with the newest read
/// timestamp, and the newest completed one.
#[derive(Default)]
struct Probes {
    inflight: Option<(u64, Probe)>,
    done: Option<(u64, i64)>,
}

#[derive(Default)]
struct CacheState {
    entries: BTreeMap<i64, CacheEntry>,
    stats: CacheStats,
}

impl SchemaCache {
    pub fn new(kv: KvClient) -> SchemaCache {
        SchemaCache {
            kv,
            state: Mutex::default(),
            max_ts: AtomicU64::new(0),
            probes: Mutex::default(),
        }
    }

    /// The schema to use for a statement reading at `start_ts`: the one whose version is
    /// fully visible at `start_ts`. Costs one point get when the cache is warm, two when the
    /// version key alone is not conclusive, and a full reload when the version is unknown.
    pub async fn info_schema_at(&self, start_ts: u64) -> Result<Arc<InfoSchema>> {
        let raw = self.raw_version_at(start_ts).await?;
        self.info_schema_with_raw(start_ts, raw).await
    }

    /// The version key `m SchemaVersionKey` at `start_ts` (one point get).
    async fn raw_version_at(&self, start_ts: u64) -> Result<i64> {
        let mut snapshot = self.kv.snapshot(start_ts);
        MetaReader::new(&mut snapshot).schema_version().await
    }

    /// A schema to plan a statement with before the schema visible at its `start_ts` is
    /// known: the newest cached one. The statement may only use results obtained with it after
    /// [`SchemaCache::check_at`] returned [`SchemaCheck::Valid`] for its `start_ts` (see
    /// ADR 0003, "Optimistic schema check").
    pub fn candidate(&self) -> Option<Arc<InfoSchema>> {
        self.latest()
    }

    /// Check that `candidate` is the schema visible at `start_ts`, by reading the version key
    /// at some timestamp `>= start_ts` and applying [`check_accepts`]. Meant to run
    /// concurrently with the statement's data read at `start_ts`.
    ///
    /// Concurrent checks share reads: a check uses the newest completed read if its timestamp
    /// is `>= start_ts`, else joins the in-flight read if that one's is, else starts a read at
    /// the newest `start_ts` seen so far (a timestamp PD has issued, so the read is a normal
    /// snapshot read), which later checks with smaller `start_ts` can join.
    pub async fn check_at(&self, candidate: &InfoSchema, start_ts: u64) -> Result<SchemaCheck> {
        self.max_ts.fetch_max(start_ts, Ordering::Relaxed);
        let (read_ts, probe) = {
            let mut probes = self.probes.lock().expect("schema probe lock");
            match (&probes.done, &probes.inflight) {
                (Some((t, raw)), _) if *t >= start_ts => {
                    let (t, raw) = (*t, *raw);
                    drop(probes);
                    return Ok(self.decide(candidate, start_ts, t, raw));
                }
                (_, Some((t, probe))) if *t >= start_ts => (*t, probe.clone()),
                _ => {
                    let read_ts = self.max_ts.load(Ordering::Relaxed).max(start_ts);
                    let kv = self.kv.clone();
                    let probe = async move {
                        let mut snapshot = kv.snapshot(read_ts);
                        MetaReader::new(&mut snapshot)
                            .schema_version()
                            .await
                            .map_err(Arc::new)
                    }
                    .boxed()
                    .shared();
                    probes.inflight = Some((read_ts, probe.clone()));
                    self.state
                        .lock()
                        .expect("schema cache lock")
                        .stats
                        .version_reads += 1;
                    (read_ts, probe)
                }
            }
        };
        let raw = probe.await.map_err(crate::Error::Shared)?;
        {
            let mut probes = self.probes.lock().expect("schema probe lock");
            if probes.done.is_none_or(|(t, _)| t < read_ts) {
                probes.done = Some((read_ts, raw));
            }
            if probes.inflight.as_ref().is_some_and(|(t, _)| *t == read_ts) {
                probes.inflight = None;
            }
        }
        Ok(self.decide(candidate, start_ts, read_ts, raw))
    }

    /// Apply [`check_accepts`] to a version key `raw` read at `read_ts >= start_ts`.
    fn decide(&self, candidate: &InfoSchema, start_ts: u64, read_ts: u64, raw: i64) -> SchemaCheck {
        debug_assert!(read_ts >= start_ts);
        let mut state = self.state.lock().expect("schema cache lock");
        if check_accepts(state.entries.get(&raw), candidate.version(), raw, start_ts) {
            state.stats.checks_passed += 1;
            SchemaCheck::Valid
        } else {
            state.stats.checks_failed += 1;
            SchemaCheck::Stale {
                raw: (read_ts == start_ts).then_some(raw),
            }
        }
    }

    /// [`SchemaCache::info_schema_at`] when the version key at `start_ts` is already known to
    /// be `raw` (saves its get).
    pub async fn info_schema_with_raw(&self, start_ts: u64, raw: i64) -> Result<Arc<InfoSchema>> {
        let mut snapshot = self.kv.snapshot(start_ts);
        let mut reader = MetaReader::new(&mut snapshot);
        if let Some(hit) = self.lookup(raw, |e| e.diff_seen_at.is_some_and(|t| t <= start_ts)) {
            return Ok(hit);
        }
        let effective = reader.effective_version(raw).await?;
        let diff_seen = (effective == raw).then_some(start_ts);
        if let Some(hit) = self.lookup(effective, |_| true) {
            self.note_diff_seen(effective, diff_seen);
            return Ok(hit);
        }
        let schema = Arc::new(InfoSchema::load(&mut snapshot).await?);
        // The load read the same snapshot, so it sees the same version.
        debug_assert_eq!(schema.version(), effective);
        self.insert(schema.clone(), diff_seen);
        Ok(schema)
    }

    /// The newest cached schema, if any.
    pub fn latest(&self) -> Option<Arc<InfoSchema>> {
        let state = self.state.lock().expect("schema cache lock");
        state.entries.values().next_back().map(|e| e.schema.clone())
    }

    pub fn stats(&self) -> CacheStats {
        self.state.lock().expect("schema cache lock").stats
    }

    fn lookup(&self, version: i64, ok: impl Fn(&CacheEntry) -> bool) -> Option<Arc<InfoSchema>> {
        let mut state = self.state.lock().expect("schema cache lock");
        let entry = state.entries.get(&version).filter(|e| ok(e))?;
        let schema = entry.schema.clone();
        state.stats.hits += 1;
        Some(schema)
    }

    fn note_diff_seen(&self, version: i64, seen_at: Option<u64>) {
        let Some(ts) = seen_at else { return };
        let mut state = self.state.lock().expect("schema cache lock");
        if let Some(e) = state.entries.get_mut(&version) {
            e.diff_seen_at = Some(e.diff_seen_at.map_or(ts, |t| t.min(ts)));
        }
    }

    fn insert(&self, schema: Arc<InfoSchema>, diff_seen_at: Option<u64>) {
        let mut state = self.state.lock().expect("schema cache lock");
        state.stats.full_loads += 1;
        let version = schema.version();
        state.entries.entry(version).or_insert(CacheEntry {
            schema,
            diff_seen_at,
        });
        while state.entries.len() > CACHE_CAPACITY {
            state.entries.pop_first();
        }
        drop(state);
        self.note_diff_seen(version, diff_seen_at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CIStr;

    fn name(s: &str) -> CIStr {
        CIStr {
            o: s.to_owned(),
            l: s.to_lowercase(),
        }
    }

    fn db(id: i64, n: &str, state: SchemaState) -> DbInfo {
        DbInfo {
            id,
            name: name(n),
            state,
            ..DbInfo::default()
        }
    }

    fn table(id: i64, n: &str, state: SchemaState) -> TableInfo {
        TableInfo {
            id,
            name: name(n),
            state,
            ..TableInfo::default()
        }
    }

    fn entry(version: i64, diff_seen_at: Option<u64>) -> CacheEntry {
        CacheEntry {
            schema: Arc::new(InfoSchema::new(version, vec![])),
            diff_seen_at,
        }
    }

    #[test]
    fn check_accepts_only_a_proven_cache_hit() {
        let seen = entry(10, Some(100));
        // Same version, diff seen before (or at) start_ts: the cached schema is the visible one
        // (whether `raw` was read at start_ts or later: the key is `raw` throughout).
        assert!(check_accepts(Some(&seen), 10, 10, 100));
        assert!(check_accepts(Some(&seen), 10, 10, 150));
        // The diff was only seen later than start_ts: at start_ts the effective version may
        // still be 9.
        assert!(!check_accepts(Some(&seen), 10, 10, 99));
        // Diff never seen (entry loaded as `raw - 1` effective): not provable from the cache.
        assert!(!check_accepts(Some(&entry(10, None)), 10, 10, 150));
        // A DDL committed between the candidate's load and start_ts.
        assert!(!check_accepts(Some(&entry(11, Some(100))), 10, 11, 150));
        // start_ts older than the candidate (a concurrent statement loaded a newer version).
        assert!(!check_accepts(Some(&seen), 11, 10, 150));
        // Version key not in the cache at all.
        assert!(!check_accepts(None, 10, 10, 150));
    }

    #[test]
    fn lookups_are_case_insensitive_and_public_only() {
        let is = InfoSchema::new(
            7,
            vec![
                (
                    db(1, "SbTest", SchemaState::Public),
                    vec![
                        table(10, "SBTest1", SchemaState::Public),
                        table(11, "creating", SchemaState::WriteOnly),
                    ],
                ),
                (db(2, "dropping", SchemaState::DeleteOnly), vec![]),
            ],
        );
        assert_eq!(is.version(), 7);
        assert_eq!(is.schema_by_name("SBTEST").unwrap().id, 1);
        assert_eq!(is.table_by_name("sbtest", "sbtest1").unwrap().id, 10);
        assert_eq!(
            is.table_by_name("SBTEST", "SbTeSt1").unwrap().name.o,
            "SBTest1"
        );
        assert!(is.table_by_name("sbtest", "creating").is_none());
        assert!(is.table_by_id(11).is_none());
        assert_eq!(is.table_by_id(10).unwrap().name.l, "sbtest1");
        assert!(is.schema_by_name("dropping").is_none());
        assert_eq!(is.tables("sbtest").len(), 1);
        assert!(is.tables("nope").is_empty());
        assert_eq!(is.schemas().count(), 1);
    }
}
