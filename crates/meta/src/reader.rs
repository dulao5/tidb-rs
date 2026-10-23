//! Reads TiDB's meta structures at one snapshot (Go: `meta.Mutator` over a snapshot).

use tidb_kv::KeyRange;
use tidb_kv::KvSnapshot;

use crate::Error;
use crate::Result;
use crate::keys;
use crate::model::DbInfo;
use crate::model::TableInfo;

/// Meta reads at the timestamp of `snapshot`.
pub struct MetaReader<'a> {
    snapshot: &'a mut KvSnapshot,
}

impl<'a> MetaReader<'a> {
    pub fn new(snapshot: &'a mut KvSnapshot) -> MetaReader<'a> {
        MetaReader { snapshot }
    }

    /// The snapshot timestamp.
    pub fn ts(&self) -> u64 {
        self.snapshot.ts()
    }

    /// The global schema version key; 0 if the cluster has never been bootstrapped
    /// (Go: `GetSchemaVersion`).
    pub async fn schema_version(&mut self) -> Result<i64> {
        let key = keys::string_data_key(keys::SCHEMA_VERSION_KEY);
        match self.snapshot.get(key).await? {
            None => Ok(0),
            Some(v) => parse_i64(&v),
        }
    }

    /// Whether the schema diff of `version` exists (Go: `GetSchemaDiff(version) != nil`).
    pub async fn schema_diff_exists(&mut self, version: i64) -> Result<bool> {
        let key = keys::string_data_key(&keys::schema_diff_key(version));
        Ok(self.snapshot.get(key).await?.is_some())
    }

    /// The schema version whose changes are completely visible at this snapshot (Go:
    /// `GetSchemaVersionWithNonEmptyDiff`).
    ///
    /// The DDL owner allocates a version in its own transaction and writes the meta changes
    /// together with the version's diff in a later one, so at a given timestamp the version
    /// key can be one ahead of the meta content. In that case version - 1 describes what is
    /// visible.
    pub async fn schema_version_with_non_empty_diff(&mut self) -> Result<i64> {
        let v = self.schema_version().await?;
        self.effective_version(v).await
    }

    /// `v` if its diff exists, else `v - 1` (for `v > 0`).
    pub(crate) async fn effective_version(&mut self, v: i64) -> Result<i64> {
        if v > 0 && !self.schema_diff_exists(v).await? {
            Ok(v - 1)
        } else {
            Ok(v)
        }
    }

    /// All databases (Go: `ListDatabases`).
    pub async fn list_databases(&mut self) -> Result<Vec<DbInfo>> {
        self.hash_get_all(keys::DBS_KEY)
            .await?
            .into_iter()
            .map(|(field, value)| {
                serde_json::from_slice::<DbInfo>(&value)
                    .map_err(|e| Error::corrupt(format!("DBInfo {}", lossy(&field)), e))
            })
            .collect()
    }

    /// All tables, views and sequences of database `db_id` (Go: `ListTables`).
    pub async fn list_tables(&mut self, db_id: i64) -> Result<Vec<TableInfo>> {
        let mut tables = Vec::new();
        for (field, value) in self.hash_get_all(&keys::db_key(db_id)).await? {
            // The database hash also holds auto-id counters etc.
            if keys::parse_table_key(&field).is_none() {
                continue;
            }
            let mut table: TableInfo = serde_json::from_slice(&value)
                .map_err(|e| Error::corrupt(format!("TableInfo {}", lossy(&field)), e))?;
            table.db_id = db_id;
            tables.push(table);
        }
        Ok(tables)
    }

    /// One table (Go: `GetTable`); `None` if absent.
    pub async fn get_table(&mut self, db_id: i64, table_id: i64) -> Result<Option<TableInfo>> {
        let key = keys::hash_data_key(&keys::db_key(db_id), &keys::table_key(table_id));
        let Some(value) = self.snapshot.get(key).await? else {
            return Ok(None);
        };
        let mut table: TableInfo = serde_json::from_slice(&value)
            .map_err(|e| Error::corrupt(format!("TableInfo {table_id}"), e))?;
        table.db_id = db_id;
        Ok(Some(table))
    }

    /// All `(field, value)` pairs of hash `key` in field order (Go: `HGetAll`).
    async fn hash_get_all(&mut self, key: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let prefix = keys::hash_data_prefix(key);
        let range = KeyRange::new(prefix.clone(), keys::prefix_next(&prefix));
        self.snapshot
            .scan_all(range)
            .await?
            .into_iter()
            .map(|(k, v)| {
                let field = keys::decode_hash_field(&k, &prefix)
                    .ok_or_else(|| Error::Corrupt(format!("bad hash data key {}", lossy(&k))))?;
                Ok((field, v))
            })
            .collect()
    }
}

fn parse_i64(v: &[u8]) -> Result<i64> {
    std::str::from_utf8(v)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Corrupt(format!("bad integer value {}", lossy(v))))
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}
