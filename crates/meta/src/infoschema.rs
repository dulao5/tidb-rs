//! An immutable snapshot of all schemas, and a per-statement versioned cache of them.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

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
}

/// Versioned cache of [`InfoSchema`]s, consulted once per statement with its `start_ts`.
///
/// See `docs/adr/0003-schema-loading.md` for why this keeps the read-only node consistent.
pub struct SchemaCache {
    kv: KvClient,
    state: Mutex<CacheState>,
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
        }
    }

    /// The schema to use for a statement reading at `start_ts`: the one whose version is
    /// fully visible at `start_ts`. Costs one point get when the cache is warm, two when the
    /// version key alone is not conclusive, and a full reload when the version is unknown.
    pub async fn info_schema_at(&self, start_ts: u64) -> Result<Arc<InfoSchema>> {
        let mut snapshot = self.kv.snapshot(start_ts);
        let mut reader = MetaReader::new(&mut snapshot);
        let raw = reader.schema_version().await?;
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
