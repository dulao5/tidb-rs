//! go-file: pkg/infoschema/infoschema_v2.go
//!
//! Infoschema v2: [`Data`], the multi-version index of all schemas and tables shared by
//! every InfoSchema version of a node, and [`InfoSchemaV2`], one version's view of it.
//!
//! Exception A3 (specs/D06.3-infoschema/spec.md §5). The business logic is Go's: the same
//! items keyed by (name or ID, schema version), tomb records, the version-visibility
//! search, the GC and full-load reset rules, the SIEVE table cache. The layout differs:
//! - each Go `atomic.Pointer[btree.BTreeG[T]]` (copy-on-write B-tree, compare-and-swap) is
//!   one `RwLock<BTreeMap<key, item>>` updated in place. The builder is the only writer
//!   (it runs under the domain's reload lock, as in Go), every item carries its schema
//!   version, and readers skip versions newer than theirs, so readers see what they would
//!   see in Go (§5.1);
//! - Go's `compareXxx` functions are the map keys;
//! - the table cache charges Rust sizes ([`crate::sieve`]);
//! - `specials` (Go `sync.Map`) is a map ordered by name (M-MAPORDER);
//! - `tidb_schema_cache_size = 0` (v1 in Go) is an unbounded cache (§5.4).

#![allow(non_upper_case_globals)]

use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::{Mutex, RwLock};
use tidb_base::Error;
use tidb_base::gofmt::Arg;
use tidb_base::terror;
use tidb_kv::option::TiKVClientReadTimeout;
use tidb_kv::version::new_version;
use tidb_model::{
    DBInfo, FKVersion1, PartitionDefinition, ReferredFKInfo, SchemaDiff, TableInfo, TableNameInfo,
};
use tidb_parser::model::CIStr;

use crate::autoid::{self, Requirement};
use crate::builder::{
    Builder, Factory, append_affected_ids, apply_create_table,
    convert_charset_collate_to_lower_case_if_need, convert_old_version_utf8_to_utf8mb4_if_need,
    drop_table_for_update, table_from_meta, table_id_is_valid,
};
use crate::bundle_builder::BundleInfoBuilder;
use crate::context::{self as infoschemacontext, SpecialAttributeFilter, TableInfoResult};
use crate::error::{ErrDatabaseExists, ErrDatabaseNotExists, ErrTableNotExists};
use crate::infoschema::{
    InfoSchemaMisc, SchemaTables, get_table_info, get_table_info_list, new_info_schema,
};
use crate::meta::{self, ErrDBNotExists, Mutator};
use crate::metadef;
use crate::sieve::{Sieve, new_sieve};
use crate::table::TableRef;

/// tableItem is the btree item sorted by name or by id.
#[derive(Clone, Debug, PartialEq)]
pub struct TableItem {
    pub db_name: CIStr,
    pub db_id: i64,
    pub table_name: CIStr,
    pub table_id: i64,
    pub schema_version: i64,
    pub tomb: bool,
}

#[derive(Clone)]
struct SchemaItem {
    schema_version: i64,
    db_info: Arc<DBInfo>,
    tomb: bool,
}

#[derive(Clone)]
struct SchemaIDName {
    schema_version: i64,
    id: i64,
    name: CIStr,
    tomb: bool,
}

#[derive(Clone)]
struct ReferredForeignKeyItem {
    schema_version: i64,
    db_name: String,
    table_name: String,
    referred_fk_info: Vec<Arc<ReferredFKInfo>>,
    tomb: bool,
}

impl SchemaItem {
    // go: schemaItem.Name L1
    fn name(&self) -> &str {
        &self.db_info.name.l
    }
}

/// versionAndTimestamp is the tuple of schema version and timestamp.
#[derive(Clone, Copy, Debug)]
struct VersionAndTimestamp {
    schema_version: i64,
    timestamp: u64,
}

/// One multi-version index (Go `atomic.Pointer[btree.BTreeG[T]]`, A3 §5.1).
type Index<K, V> = RwLock<BTreeMap<K, V>>;

/// btreeSet updates the btree: insert, or replace the item with the same key.
// go: btreeSet L2 (A3: one map under a lock, updated in place, not a copy-on-write B-tree behind an atomic pointer)
fn btree_set<K: Ord, V>(ptr: &Index<K, V>, key: K, item: V) {
    ptr.write().insert(key, item);
}

type NameKey = (String, String, i64);
type IdKey = (i64, i64);
type SchemaKey = (String, i64);
type FkKey = (String, String, i64);
type ResidentKey = (String, i64, i64);

/// Data is the core data struct of infoschema V2.
pub struct Data {
    /// For the TableByName API, sorted by {dbName, tableName, schemaVersion} => tableID
    ///
    /// If the schema version +1 but a specific table does not change, the old record is
    /// kept and no new {dbName, tableName, schemaVersion+1} => tableID record been added.
    ///
    /// It means as long as we can find an item in it, the item is available, even through
    /// the schema version maybe smaller than required.
    by_name: Index<NameKey, TableItem>,

    /// For the TableByID API, sorted by {tableID, schemaVersion} => dbID
    /// To reload model.TableInfo, we need both table ID and database ID for meta kv API.
    /// It provides the tableID => databaseID mapping.
    /// This mapping MUST be synced with byName.
    by_id: Index<IdKey, TableItem>,

    /// For the SchemaByName API, sorted by {dbName, schemaVersion} => model.DBInfo
    /// Stores the full data in memory.
    schema_map: Index<SchemaKey, SchemaItem>,

    /// For the SchemaByID API, sorted by {id, schemaVersion}
    /// Stores only id, name and schemaVersion in memory.
    schema_id2_name: Index<IdKey, SchemaIDName>,

    /// referredForeignKeys records all table's ReferredFKInfo.
    referred_foreign_keys: Index<FkKey, ReferredForeignKeyItem>,

    pub(crate) table_cache: Sieve<TableCacheKey, TableRef>,

    /// sorted by both SchemaVersion and timestamp in descending order, assume they have same
    /// order
    version_timestamps: RwLock<Vec<VersionAndTimestamp>>,

    /// For information_schema/metrics_schema/performance_schema etc
    specials: RwLock<BTreeMap<String, Arc<SchemaTables>>>,

    /// pid2tid is used by FindTableInfoByPartitionID, it stores {partitionID,
    /// schemaVersion} => table ID. Need full data in memory!
    pid2tid: Index<IdKey, PartitionItem>,

    /// tableInfoResident stores {dbName, tableID, schemaVersion} => model.TableInfo
    /// It is part of the model.TableInfo data kept in memory to accelerate the list tables
    /// API. We observe the pattern that list table API always come with filter.
    /// All model.TableInfo with special attributes are here, currently the special
    /// attributes including: TTLInfo, TiFlashReplica
    /// PlacementPolicyRef, Partition might be added later, and also TableLock etc
    table_info_resident: Index<ResidentKey, TableInfoItem>,

    /// the minimum ts of the recent used infoschema (read and reset by `InfoCache`)
    pub recent_min_ts: AtomicU64,
}

#[derive(Clone)]
struct TableInfoItem {
    db_name: CIStr,
    table_id: i64,
    schema_version: i64,
    table_info: Option<Arc<TableInfo>>,
    tomb: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PartitionItem {
    partition_id: i64,
    schema_version: i64,
    table_id: i64,
    tomb: bool,
}

/// The key of the table cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TableCacheKey {
    pub table_id: i64,
    pub schema_version: i64,
}

impl crate::sieve::SieveSize for TableCacheKey {
    // glue: A3
    fn heap_size(&self) -> u64 {
        0
    }
}

/// The schema cache size Go uses for `tidb_schema_cache_size` (A3 §5.4): 0 switches Go to
/// infoschema v1, which keeps every table in memory; tidb-rs has only v2 and keeps every
/// table by an unbounded cache instead.
// glue: A3 (tidb_schema_cache_size = 0)
pub fn schema_cache_capacity(size: u64) -> u64 {
    if size == 0 { u64::MAX } else { size }
}

impl Data {
    // go: Data.getVersionByTS L1 [M-LOCK]
    #[allow(dead_code)]
    fn get_version_by_ts(&self, ts: u64) -> (i64, bool) {
        let mu = self.version_timestamps.read();
        Self::get_version_by_ts_no_lock(&mu, ts)
    }

    // go: Data.getVersionByTSNoLock L1
    fn get_version_by_ts_no_lock(
        version_timestamps: &[VersionAndTimestamp],
        ts: u64,
    ) -> (i64, bool) {
        // search one by one instead of binary search, because the timestamp of a schema could be 0
        // this is ok because the size of h.tableCache is small (currently set to 16)
        // moreover, the most likely hit element in the array is the first one in steady mode
        // thus it may have better performance than binary search
        for (i, vt) in version_timestamps.iter().enumerate() {
            if vt.timestamp == 0 || ts < vt.timestamp {
                // is.timestamp == 0 means the schema ts is unknown, so we can't use it, then just skip it.
                // ts < is.timestamp means the schema is newer than ts, so we can't use it too, just skip it to find the older one.
                continue;
            }
            // ts >= is.timestamp must be true after the above condition.
            if i == 0 {
                // the first element is the latest schema, so we can return it directly.
                return (vt.schema_version, true);
            }
            if version_timestamps[i - 1].schema_version == vt.schema_version + 1
                && version_timestamps[i - 1].timestamp > ts
            {
                // This first condition is to make sure the schema version is continuous. If last(cache[i-1]) schema-version is 10,
                // but current(cache[i]) schema-version is not 9, then current schema is not suitable for ts.
                // The second condition is to make sure the cache[i-1].timestamp > ts >= cache[i].timestamp, then the current schema is suitable for ts.
                return (vt.schema_version, true);
            }
            // current schema is not suitable for ts, then break the loop to avoid the unnecessary search.
            break;
        }

        (0, false)
    }
}

const BTREE_DEGREE: usize = 16;

/// NewData creates an infoschema V2 data struct. Go installs the metrics hook
/// (`newSieveStatusHookImpl`, block D06.6); until then the cache keeps the empty hook.
// go: NewData L2 (A3: maps instead of B-trees of degree 16; the metrics hook is D06.6)
pub fn new_data() -> Arc<Data> {
    let _ = BTREE_DEGREE;
    Arc::new(Data {
        by_name: RwLock::new(BTreeMap::new()),
        by_id: RwLock::new(BTreeMap::new()),
        schema_map: RwLock::new(BTreeMap::new()),
        schema_id2_name: RwLock::new(BTreeMap::new()),
        referred_foreign_keys: RwLock::new(BTreeMap::new()),
        table_cache: new_sieve(1024 * 1024 * (1 << 20)),
        version_timestamps: RwLock::new(Vec::new()),
        specials: RwLock::new(BTreeMap::new()),
        pid2tid: RwLock::new(BTreeMap::new()),
        table_info_resident: RwLock::new(BTreeMap::new()),
        recent_min_ts: AtomicU64::new(0),
    })
}

impl Data {
    /// CacheCapacity is exported for testing.
    // go: Data.CacheCapacity L1
    pub fn cache_capacity(&self) -> u64 {
        self.table_cache.capacity()
    }

    /// SetCacheCapacity sets the cache capacity size in bytes (`tidb_schema_cache_size`;
    /// 0 is unbounded, A3 §5.4).
    // go: Data.SetCacheCapacity L2 (A3: capacity 0 is unbounded)
    pub fn set_cache_capacity(&self, capacity: u64) {
        self.table_cache
            .set_capacity_and_wait_evict(schema_cache_capacity(capacity));
    }

    // go: Data.add L1
    pub fn add(&self, item: TableItem, tbl: TableRef) {
        btree_set(&self.by_id, by_id_key(&item), item.clone());
        btree_set(&self.by_name, by_name_key(&item), item.clone());
        self.table_cache.set(
            TableCacheKey {
                table_id: item.table_id,
                schema_version: item.schema_version,
            },
            tbl.clone(),
        );
        let ti = tbl.meta();
        if let Some(pi) = ti.get_partition_info() {
            for def in pi.definitions.iter().flatten() {
                let p = PartitionItem {
                    partition_id: def.id,
                    schema_version: item.schema_version,
                    table_id: tbl.meta().id,
                    tomb: false,
                };
                btree_set(&self.pid2tid, partition_item_key(&p), p);
            }
        }
        if infoschemacontext::has_special_attributes(ti) {
            let r = TableInfoItem {
                db_name: item.db_name.clone(),
                table_id: item.table_id,
                schema_version: item.schema_version,
                table_info: Some(ti.clone()),
                tomb: false,
            };
            btree_set(&self.table_info_resident, table_info_item_key(&r), r);
        }
    }

    /// The name-to-ID items of the tables a full load does not read (Go's
    /// `btreeSet(&b.infoData.byID, &item)` / `byName` in `createSchemaTablesForDB`).
    // go-part: Builder.createSchemaTablesForDB
    pub fn add_name_only(&self, item: TableItem) {
        btree_set(&self.by_id, by_id_key(&item), item.clone());
        btree_set(&self.by_name, by_name_key(&item), item);
    }

    /// LoadOrStore: an existing entry is kept.
    // go: Data.addSpecialDB L1
    pub fn add_special_db(&self, di: &DBInfo, tables: Arc<SchemaTables>) {
        self.specials
            .write()
            .entry(di.name.l.clone())
            .or_insert(tables);
    }

    // go: Data.addDB L1
    pub fn add_db(&self, schema_version: i64, mut db_info: DBInfo) {
        db_info.deprecated.tables = None;
        self.add_db_arc(schema_version, Arc::new(db_info));
    }

    /// `Data.addDB` with a `*model.DBInfo` the InfoSchema already holds (its tables are
    /// already nil).
    // go-part: Data.addDB
    pub fn add_db_arc(&self, schema_version: i64, db_info: Arc<DBInfo>) {
        let n = SchemaIDName {
            schema_version,
            id: db_info.id,
            name: db_info.name.clone(),
            tomb: false,
        };
        btree_set(&self.schema_id2_name, schema_id_name_key(&n), n);
        let s = SchemaItem {
            schema_version,
            db_info,
            tomb: false,
        };
        btree_set(&self.schema_map, schema_item_key(&s), s);
    }

    // go: Data.remove L1
    pub fn remove(&self, mut item: TableItem) {
        item.tomb = true;
        btree_set(&self.by_id, by_id_key(&item), item.clone());
        btree_set(&self.by_name, by_name_key(&item), item.clone());
        let r = TableInfoItem {
            db_name: item.db_name.clone(),
            table_id: item.table_id,
            schema_version: item.schema_version,
            table_info: None,
            tomb: true,
        };
        btree_set(&self.table_info_resident, table_info_item_key(&r), r);
        self.table_cache.remove(&TableCacheKey {
            table_id: item.table_id,
            schema_version: item.schema_version,
        });
    }

    // go: Data.deleteDB L1
    pub fn delete_db(&self, db_info: Arc<DBInfo>, schema_version: i64) {
        let n = SchemaIDName {
            schema_version,
            id: db_info.id,
            name: db_info.name.clone(),
            tomb: true,
        };
        let item = SchemaItem {
            schema_version,
            db_info,
            tomb: true,
        };
        btree_set(&self.schema_map, schema_item_key(&item), item);
        btree_set(&self.schema_id2_name, schema_id_name_key(&n), n);
    }

    /// Go mutates the `*model.DBInfo` that `SchemaByID` returned, which every schema item
    /// holding that pointer shares (older versions included). Rust replaces the `Arc` in
    /// those items, so later lookups see what they see in Go.
    // go-part: Builder.applyModifySchemaCharsetAndCollateV2
    fn replace_shared_db_info(&self, old: &Arc<DBInfo>, new: &Arc<DBInfo>) {
        let mut m = self.schema_map.write();
        let lo = (old.name.l.clone(), i64::MIN);
        let hi = (old.name.l.clone(), i64::MAX);
        for (_, item) in m.range_mut(lo..=hi) {
            if Arc::ptr_eq(&item.db_info, old) {
                item.db_info = new.clone();
            }
        }
    }
}

struct ReferredForeignKeysHelper {
    start: ReferredForeignKeyItem,
    schema_version: i64,
    referred_fk_infos: Vec<Arc<ReferredFKInfo>>,
}

impl ReferredForeignKeysHelper {
    // go: referredForeignKeysHelper.onItem L1
    fn on_item(&mut self, item: &ReferredForeignKeyItem) -> bool {
        if item.db_name != self.start.db_name || item.table_name != self.start.table_name {
            return false;
        }

        if item.schema_version <= self.schema_version {
            if !item.tomb {
                // If the item is a tomb record, all the foreign keys are deleted.
                self.referred_fk_infos = item.referred_fk_info.clone();
            }
            return false;
        }
        true
    }
}

impl Data {
    // go: Data.getTableReferredForeignKeys L1
    pub fn get_table_referred_foreign_keys(
        &self,
        schema: &str,
        table: &str,
        schema_meta_version: i64,
    ) -> Vec<Arc<ReferredFKInfo>> {
        let mut helper = ReferredForeignKeysHelper {
            start: ReferredForeignKeyItem {
                schema_version: i64::MAX,
                db_name: schema.to_owned(),
                table_name: table.to_owned(),
                referred_fk_info: Vec::new(),
                tomb: false,
            },
            schema_version: schema_meta_version,
            referred_fk_infos: Vec::new(),
        };
        let bt = self.referred_foreign_keys.read();
        for (_, item) in bt
            .range(..=referred_foreign_key_item_key(&helper.start))
            .rev()
        {
            if !helper.on_item(item) {
                break;
            }
        }
        helper.referred_fk_infos
    }

    /// hasForeignKeyReference checks if a specific foreign key reference already exists
    // go: Data.hasForeignKeyReference L1
    fn has_foreign_key_reference(
        &self,
        refs: &[Arc<ReferredFKInfo>],
        schema: &CIStr,
        table: &CIStr,
        fk_name: &CIStr,
    ) -> bool {
        for r in refs {
            if r.child_schema.l == schema.l
                && r.child_table.l == table.l
                && r.child_fk_name.l == fk_name.l
            {
                return true;
            }
        }
        false
    }

    // go: Data.addReferredForeignKeys L1
    pub fn add_referred_foreign_keys(
        &self,
        schema: &CIStr,
        tb_info: &TableInfo,
        schema_meta_version: i64,
    ) {
        for fk in tb_info.foreign_keys.iter().flatten() {
            if fk.version < FKVersion1 {
                continue;
            }

            // Get current foreign key references for the table
            let (ref_schema, ref_table) = (&fk.ref_schema.l, &fk.ref_table.l);
            let existing_refs = self.get_table_referred_foreign_keys(
                &fk.ref_schema.l,
                &fk.ref_table.l,
                schema_meta_version,
            );

            // Skip if this specific foreign key reference already exists
            if self.has_foreign_key_reference(&existing_refs, schema, &tb_info.name, &fk.name) {
                continue;
            }

            // Create a new array with existing refs + new reference
            let mut new_refs = Vec::with_capacity(existing_refs.len() + 1);
            new_refs.extend(existing_refs);
            new_refs.push(Arc::new(ReferredFKInfo {
                cols: fk.ref_cols.clone(),
                child_schema: schema.clone(),
                child_table: tb_info.name.clone(),
                child_fk_name: fk.name.clone(),
            }));
            // sort.Slice is not stable; the three keys of the existing refs are distinct,
            // so any sort gives Go's order.
            new_refs.sort_by(|a, b| {
                (&a.child_schema.l, &a.child_table.l, &a.child_fk_name.l).cmp(&(
                    &b.child_schema.l,
                    &b.child_table.l,
                    &b.child_fk_name.l,
                ))
            });
            let item = ReferredForeignKeyItem {
                db_name: ref_schema.clone(),
                table_name: ref_table.clone(),
                schema_version: schema_meta_version,
                referred_fk_info: new_refs,
                tomb: false,
            };
            btree_set(
                &self.referred_foreign_keys,
                referred_foreign_key_item_key(&item),
                item,
            );
        }
    }

    // go: Data.deleteReferredForeignKeys L1
    pub fn delete_referred_foreign_keys(
        &self,
        schema: &CIStr,
        tb_info: &TableInfo,
        schema_meta_version: i64,
    ) {
        for fk in tb_info.foreign_keys.iter().flatten() {
            if fk.version < FKVersion1 {
                continue;
            }

            // Get current foreign key references for the table
            let (ref_schema, ref_table) = (&fk.ref_schema.l, &fk.ref_table.l);
            let existing_refs =
                self.get_table_referred_foreign_keys(ref_schema, ref_table, schema_meta_version);

            // Skip if this specific foreign key reference doesn't exist
            if !self.has_foreign_key_reference(&existing_refs, schema, &tb_info.name, &fk.name) {
                continue;
            }

            // Delete the reference
            let item = if existing_refs.len() == 1 {
                // If this is the only reference, mark the whole item as deleted
                ReferredForeignKeyItem {
                    db_name: ref_schema.clone(),
                    table_name: ref_table.clone(),
                    schema_version: schema_meta_version,
                    tomb: true,
                    referred_fk_info: Vec::new(),
                }
            } else {
                // If there are multiple references, create new array excluding this one
                // clone existingRefs to avoid modifying the original slice
                let new_refs: Vec<_> = existing_refs
                    .into_iter()
                    .filter(|r| {
                        !(r.child_schema.l == schema.l
                            && r.child_table.l == tb_info.name.l
                            && r.child_fk_name.l == fk.name.l)
                    })
                    .collect();
                ReferredForeignKeyItem {
                    db_name: ref_schema.clone(),
                    table_name: ref_table.clone(),
                    schema_version: schema_meta_version,
                    tomb: false,
                    referred_fk_info: new_refs,
                }
            };
            btree_set(
                &self.referred_foreign_keys,
                referred_foreign_key_item_key(&item),
                item,
            );
        }
    }
}

/// gcCollectTableItem returns up to maxItems old tableItem versions for GC.
///
/// Deliberate deviation (tidb-rs #195, docs/compatibility.md): Go deletes every item older
/// than the cut whose previous (newer) item is the same table, without requiring that newer
/// item to be at or below the cut. A table created at v3 and altered at v8, GC'd with cut 5
/// (`meta.GetOldestSchemaVersion`, the oldest version in use), loses its v3 item, and an
/// InfoSchema at v5 (a stale read, `tidb_snapshot`) then reports "table doesn't exist".
/// tidb-rs keeps, per table, the newest item at or below the cut (what a reader at the cut
/// uses) and deletes only the items older than it.
// go: gcCollectTableItem L2 (deviation #195: keep the newest item <= cut of each table)
fn gc_collect_table_item(
    bt: &BTreeMap<NameKey, TableItem>,
    cut_ver: i64,
    max_items: usize,
) -> Vec<TableItem> {
    let mut dels = Vec::new();
    let mut prev: Option<&TableItem> = None;
    // Whether the current table's newest item at or below the cut has been seen.
    let mut anchored = false;
    // Example:
    // gcOldVersion to v4
    //	db3 tbl1 v5
    //	db3 tbl1 v4    <- kept: the newest item <= v4
    //	db3 tbl1 v3    <- delete, older than the kept v4
    //	db2 tbl2 v1    <- keep, the newest item <= v4
    //	db2 tbl2 v0    <- delete
    //	db1 tbl3 v8
    //	db1 tbl3 v3    <- keep (Go deletes it: v3 < v4 and the previous item is the same table)
    for item in bt.values().rev() {
        let same = prev
            .is_some_and(|p| p.db_name.l == item.db_name.l && p.table_name.l == item.table_name.l);
        if !same {
            anchored = false;
        }
        if anchored {
            dels.push(item.clone());
            if dels.len() >= max_items {
                break;
            }
        }
        if item.schema_version <= cut_ver {
            anchored = true;
        }
        prev = Some(item);
    }
    dels
}

/// gcCollectReferredForeignKeyItem returns up to maxItems old referredForeignKeyItem
/// versions for GC. Same deliberate deviation as [`gc_collect_table_item`] (#195).
// go: gcCollectReferredForeignKeyItem L2 (deviation #195: keep the newest item <= cut of each table)
fn gc_collect_referred_foreign_key_item(
    bt: &BTreeMap<FkKey, ReferredForeignKeyItem>,
    cut_ver: i64,
    max_items: usize,
) -> Vec<FkKey> {
    let mut dels = Vec::new();
    let mut prev: Option<&ReferredForeignKeyItem> = None;
    let mut anchored = false;
    for (k, item) in bt.iter().rev() {
        let same =
            prev.is_some_and(|p| p.db_name == item.db_name && p.table_name == item.table_name);
        if !same {
            anchored = false;
        }
        if anchored {
            dels.push(k.clone());
            if dels.len() >= max_items {
                break;
            }
        }
        if item.schema_version <= cut_ver {
            anchored = true;
        }
        prev = Some(item);
    }
    dels
}

impl Data {
    /// gcOldFKVersion performs GC of old referredForeignKeyItem entries up to maxItems.
    // go: Data.gcOldFKVersion L2 (A3: deleted under the lock; Go's compare-and-swap cannot conflict)
    fn gc_old_fk_version(&self, schema_version: i64) -> usize {
        let mut rf = self.referred_foreign_keys.write();
        let rf_dels = gc_collect_referred_foreign_key_item(&rf, schema_version, 1024);
        for fk in &rf_dels {
            rf.remove(fk);
        }
        rf_dels.len()
    }

    /// GCOldVersion compacts btree nodes by removing items older than schema version.
    /// exported for testing
    // go: Data.GCOldVersion L2 (A3: deleted under the locks; Go's compare-and-swap cannot conflict)
    pub fn gc_old_version(&self, schema_version: i64) -> (usize, i64) {
        if self.by_name.read().is_empty() {
            return (0, 0);
        }

        // collect and remove old tableItems
        let dels = {
            let mut by_name = self.by_name.write();
            let mut by_id = self.by_id.write();
            let dels = gc_collect_table_item(&by_name, schema_version, 1024);
            for ti in &dels {
                by_name.remove(&by_name_key(ti));
                by_id.remove(&by_id_key(ti));
            }
            dels
        };

        // collect and remove old referredForeignKeyItems
        let _ = self.gc_old_fk_version(schema_version);

        (dels.len(), self.by_name.read().len() as i64)
    }

    /// resetBeforeFullLoad is called before a full recreate operation within
    /// builder.InitWithDBInfos().
    // go: Data.resetBeforeFullLoad L1
    pub fn reset_before_full_load(&self, schema_version: i64) {
        reset_table_info_resident_before_full_load(&self.table_info_resident, schema_version);

        reset_by_id_before_full_load(&self.by_id, schema_version);
        reset_by_name_before_full_load(&self.by_name, schema_version);

        reset_schema_map_before_full_load(&self.schema_map, schema_version);
        reset_schema_id2_name_before_full_load(&self.schema_id2_name, schema_version);

        reset_pid2_tid_before_full_load(&self.pid2tid, schema_version);
        reset_fk_before_full_load(&self.referred_foreign_keys, schema_version);
    }
}

/// The items of `bt` a Go reset function marks, in its (descending) order: the greatest
/// item, then every item not `same` as the last one taken. Go walks the tree it loaded
/// before writing (its writes go to new copies); Rust collects first, then writes. Go's
/// batching (1000 items per walk) gives the same items.
// go-part: resetByIDBeforeFullLoad
fn reset_pivots<K: Ord, V: Clone>(bt: &BTreeMap<K, V>, same: impl Fn(&V, &V) -> bool) -> Vec<V> {
    let mut items: Vec<V> = Vec::new();
    for item in bt.values().rev() {
        match items.last() {
            Some(pivot) if same(pivot, item) => {} // skip MVCC version
            _ => items.push(item.clone()),
        }
    }
    items
}

// go: resetByIDBeforeFullLoad L2 (A3: collects the items, then writes; same items as Go's batches)
fn reset_by_id_before_full_load(ptr: &Index<IdKey, TableItem>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| p.table_id == i.table_id);
    for item in items {
        let t = TableItem {
            db_name: item.db_name,
            db_id: item.db_id,
            table_name: item.table_name,
            table_id: item.table_id,
            schema_version,
            tomb: true,
        };
        btree_set(ptr, by_id_key(&t), t);
    }
}

// go: resetByNameBeforeFullLoad L2 (A3: collects the items, then writes; same items as Go's batches)
fn reset_by_name_before_full_load(ptr: &Index<NameKey, TableItem>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| {
        p.db_name == i.db_name && p.table_name == i.table_name
    });
    for item in items {
        let t = TableItem {
            db_name: item.db_name,
            db_id: item.db_id,
            table_name: item.table_name,
            table_id: item.table_id,
            schema_version,
            tomb: true,
        };
        btree_set(ptr, by_name_key(&t), t);
    }
}

// go: resetTableInfoResidentBeforeFullLoad L2 (A3: collects the items, then writes)
fn reset_table_info_resident_before_full_load(
    ptr: &Index<ResidentKey, TableInfoItem>,
    schema_version: i64,
) {
    let items = reset_pivots(&ptr.read(), |p, i| {
        p.db_name == i.db_name && p.table_id == i.table_id
    });
    for item in items {
        let t = TableInfoItem {
            db_name: item.db_name,
            table_id: item.table_id,
            schema_version,
            table_info: None,
            tomb: true,
        };
        btree_set(ptr, table_info_item_key(&t), t);
    }
}

// go: resetSchemaMapBeforeFullLoad L2 (A3: collects the items, then writes)
fn reset_schema_map_before_full_load(ptr: &Index<SchemaKey, SchemaItem>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| p.name() == i.name());
    for item in items {
        let s = SchemaItem {
            db_info: item.db_info,
            schema_version,
            tomb: true,
        };
        btree_set(ptr, schema_item_key(&s), s);
    }
}

// go: resetSchemaID2NameBeforeFullLoad L2 (A3: collects the items, then writes)
fn reset_schema_id2_name_before_full_load(ptr: &Index<IdKey, SchemaIDName>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| p.id == i.id);
    for item in items {
        let s = SchemaIDName {
            id: item.id,
            name: item.name,
            schema_version,
            tomb: true,
        };
        btree_set(ptr, schema_id_name_key(&s), s);
    }
}

// go: resetPID2TIDBeforeFullLoad L2 (A3: collects the items, then writes; same items as Go's batches)
fn reset_pid2_tid_before_full_load(ptr: &Index<IdKey, PartitionItem>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| p.partition_id == i.partition_id);
    for item in items {
        let p = PartitionItem {
            partition_id: item.partition_id,
            table_id: item.table_id,
            schema_version,
            tomb: true,
        };
        btree_set(ptr, partition_item_key(&p), p);
    }
}

// go: resetFKBeforeFullLoad L2 (A3: collects the items, then writes)
fn reset_fk_before_full_load(ptr: &Index<FkKey, ReferredForeignKeyItem>, schema_version: i64) {
    let items = reset_pivots(&ptr.read(), |p, i| {
        p.db_name == i.db_name && p.table_name == i.table_name
    });
    for item in items {
        let f = ReferredForeignKeyItem {
            db_name: item.db_name,
            table_name: item.table_name,
            schema_version,
            tomb: true,
            referred_fk_info: Vec::new(),
        };
        btree_set(ptr, referred_foreign_key_item_key(&f), f);
    }
}

// go: compareByID L2 (A3: the order is the map key)
fn by_id_key(a: &TableItem) -> IdKey {
    (a.table_id, a.schema_version)
}

// go: compareByName L2 (A3: the order is the map key)
fn by_name_key(a: &TableItem) -> NameKey {
    (
        a.db_name.l.clone(),
        a.table_name.l.clone(),
        a.schema_version,
    )
}

// go: compareTableInfoItem L2 (A3: the order is the map key)
fn table_info_item_key(a: &TableInfoItem) -> ResidentKey {
    (a.db_name.l.clone(), a.table_id, a.schema_version)
}

// go: comparePartitionItem L2 (A3: the order is the map key)
fn partition_item_key(a: &PartitionItem) -> IdKey {
    (a.partition_id, a.schema_version)
}

// go: compareSchemaItem L2 (A3: the order is the map key)
fn schema_item_key(a: &SchemaItem) -> SchemaKey {
    (a.name().to_owned(), a.schema_version)
}

// go: compareSchemaByID L2 (A3: the order is the map key)
fn schema_id_name_key(a: &SchemaIDName) -> IdKey {
    (a.id, a.schema_version)
}

// go: compareReferredForeignKeyItem L2 (A3: the order is the map key)
fn referred_foreign_key_item_key(a: &ReferredForeignKeyItem) -> FkKey {
    (a.db_name.clone(), a.table_name.clone(), a.schema_version)
}

/// The values of Go's `context.Context` the v2 lookups read: the refill option
/// ([`with_refill_option`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ctx {
    refill: Option<bool>,
}

impl Ctx {
    /// `context.Background()`.
    // glue: Go context.Background (M-GOCTX)
    pub fn background() -> Ctx {
        Ctx::default()
    }
}

/// InfoSchema v2: one schema version's view of the shared [`Data`].
#[derive(Clone)]
pub struct InfoSchemaV2 {
    /// Go embeds `*infoSchema`; v2 uses only its `infoSchemaMisc`.
    pub(crate) misc: Arc<InfoSchemaMisc>,
    pub(crate) r: Option<Arc<dyn Requirement>>,
    pub(crate) factory: Option<Factory>,
    pub(crate) ts: u64,
    pub(crate) data: Arc<Data>,
}

/// NewInfoSchemaV2 create infoschemaV2.
// go: NewInfoSchemaV2 L1
pub fn new_info_schema_v2(
    r: Option<Arc<dyn Requirement>>,
    factory: Option<Factory>,
    info_data: Arc<Data>,
) -> InfoSchemaV2 {
    InfoSchemaV2 {
        misc: Arc::new(new_info_schema()),
        data: info_data,
        r,
        factory,
        ts: 0,
    }
}

/// Go `search(bt, schemaVersion, end, matchFn)`: the latest item matching `end` whose
/// schema version is at most `schema_version`; not ok if it is a tomb.
// go: search L1
fn search<K: Ord>(
    bt: &BTreeMap<K, TableItem>,
    schema_version: i64,
    end_key: K,
    end: &TableItem,
    match_fn: impl Fn(&TableItem, &TableItem) -> bool,
) -> (Option<TableItem>, bool) {
    let mut ok = false;
    let mut target: Option<&TableItem> = None;
    // Iterate through the btree, find the query item whose schema version is the largest one (latest).
    for item in bt.range(..=end_key).rev().map(|(_, v)| v) {
        if !match_fn(end, item) {
            break;
        }
        if item.schema_version > schema_version {
            // We're seaching historical snapshot, and this record is newer than us, we can't use it.
            // Skip the record.
            continue;
        }
        // schema version of the items should <= query's schema version.
        match target {
            None => {
                // The first one found.
                ok = true;
                target = Some(item);
            }
            Some(t) => {
                // The latest one
                if item.schema_version > t.schema_version {
                    target = Some(item);
                }
            }
        }
    }
    if ok && target.is_some_and(|t| t.tomb) {
        // If the item is a tomb record, the table is dropped.
        ok = false;
    }
    (target.cloned(), ok)
}

/// TableItem is exported from tableItem.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableItemOut {
    pub db_name: CIStr,
    pub table_name: CIStr,
}

/// The `singleflight.Group` of `loadTableInfo`: one load per key at a time.
type LoadResult = Result<Option<TableRef>, Error>;
static LOAD_TABLE_SF: LazyLock<Mutex<HashMap<String, Shared<BoxFuture<'static, LoadResult>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl InfoSchemaV2 {
    /// The schema version (Go reads `is.infoSchema.schemaMetaVersion`).
    // glue: field access of the embedded infoSchemaMisc
    fn schema_meta_version(&self) -> i64 {
        self.misc.schema_meta_version
    }

    // go: infoschemaV2.base L1
    pub fn base(&self) -> &InfoSchemaMisc {
        &self.misc
    }

    /// The shared data (Go: the embedded `*Data`).
    // glue: field access of the embedded *Data
    pub fn data(&self) -> &Arc<Data> {
        &self.data
    }

    // go: infoschemaV2.CloneAndUpdateTS L1
    pub fn clone_and_update_ts(&self, start_ts: u64) -> InfoSchemaV2 {
        let mut tmp = self.clone();
        tmp.ts = start_ts;
        tmp
    }

    // go: infoschemaV2.searchTableItemByID L1
    fn search_table_item_by_id(&self, table_id: i64) -> (Option<TableItem>, bool) {
        let eq = |a: &TableItem, b: &TableItem| a.table_id == b.table_id;
        let end = TableItem {
            db_name: CIStr::default(),
            db_id: 0,
            table_name: CIStr::default(),
            table_id,
            schema_version: i64::MAX,
            tomb: false,
        };
        search(
            &self.data.by_id.read(),
            self.schema_meta_version(),
            by_id_key(&end),
            &end,
            eq,
        )
    }

    /// TableByID implements the InfoSchema interface.
    /// As opposed to TableByName, TableByID will not refill cache when schema cache miss,
    /// unless the caller changes the behavior by passing a context use WithRefillOption.
    // go: infoschemaV2.TableByID L1
    pub async fn table_by_id(&self, ctx: Ctx, id: i64) -> Option<TableRef> {
        if !table_id_is_valid(id) {
            return None;
        }

        self.keep_alive();
        let (itm, ok) = self.search_table_item_by_id(id);
        if !ok {
            return None;
        }
        let itm = itm.expect("found");

        if autoid::is_mem_schema_id(id) {
            if let Some(sch_tbls) = self.data.specials.read().get(&itm.db_name.l) {
                return sch_tbls.tables.get(&itm.table_name.l).cloned();
            }
            return None;
        }

        let refill = ctx.refill.unwrap_or(false);

        // get cache with item key
        let key = TableCacheKey {
            table_id: itm.table_id,
            schema_version: itm.schema_version,
        };
        if let Some(tbl) = self.data.table_cache.get(&key) {
            return Some(tbl);
        }

        // Maybe the table is evicted? need to reload.
        let ret = match self
            .load_table_info(ctx, id, itm.db_id, self.ts, self.schema_meta_version())
            .await
        {
            Ok(r) => r,
            Err(_) => return None,
        };

        if refill {
            self.data.table_cache.set(key, ret.clone());
        }
        Some(ret)
    }

    /// TableItemByID implements the InfoSchema interface.
    /// It only contains memory operations, no worries about accessing the storage.
    // go: infoschemaV2.TableItemByID L1
    pub fn table_item_by_id(&self, table_id: i64) -> Option<TableItemOut> {
        if !table_id_is_valid(table_id) {
            return None;
        }
        let (itm, ok) = self.search_table_item_by_id(table_id);
        if !ok {
            return None;
        }
        let itm = itm.expect("found");
        Some(TableItemOut {
            db_name: itm.db_name,
            table_name: itm.table_name,
        })
    }

    /// IterateAllTableItems is used for special performance optimization.
    /// Used by executor/infoschema_reader.go to handle reading from
    /// INFORMATION_SCHEMA.TABLES. If visit return false, stop the iterate process.
    // go: infoschemaV2.IterateAllTableItems L1
    pub fn iterate_all_table_items(&self, mut visit: impl FnMut(TableItemOut) -> bool) {
        let bt = self.data.by_name.read();
        let mut pivot: Option<&TableItem> = None;
        for item in bt.values().rev() {
            if item.schema_version > self.schema_meta_version() {
                // skip MVCC version, those items are not visible to the queried schema version
                continue;
            }
            if let Some(p) = pivot
                && p.db_name == item.db_name
                && p.table_name == item.table_name
            {
                // skip MVCC version, this db.table has been visited already
                continue;
            }
            pivot = Some(item);
            if !item.tomb
                && !visit(TableItemOut {
                    db_name: item.db_name.clone(),
                    table_name: item.table_name.clone(),
                })
            {
                return;
            }
        }
    }

    /// TableIsCached checks whether the table is cached.
    // go: infoschemaV2.TableIsCached L1
    pub fn table_is_cached(&self, id: i64) -> bool {
        if !table_id_is_valid(id) {
            return false;
        }

        let (itm, ok) = self.search_table_item_by_id(id);
        if !ok {
            return false;
        }
        let itm = itm.expect("found");

        if autoid::is_mem_schema_id(id) {
            if let Some(sch_tbls) = self.data.specials.read().get(&itm.db_name.l) {
                return sch_tbls.tables.contains_key(&itm.table_name.l);
            }
            return false;
        }

        let key = TableCacheKey {
            table_id: itm.table_id,
            schema_version: itm.schema_version,
        };
        self.data.table_cache.get(&key).is_some()
    }

    /// EvictTable is exported for testing only.
    // go: infoschemaV2.EvictTable L1
    pub fn evict_table(&self, schema: &CIStr, tbl: &CIStr) {
        let eq =
            |a: &TableItem, b: &TableItem| a.db_name == b.db_name && a.table_name == b.table_name;
        let end = TableItem {
            db_name: schema.clone(),
            db_id: 0,
            table_name: tbl.clone(),
            table_id: 0,
            schema_version: i64::MAX,
            tomb: false,
        };
        let (itm, ok) = search(
            &self.data.by_name.read(),
            self.schema_meta_version(),
            by_name_key(&end),
            &end,
            eq,
        );
        if !ok {
            return;
        }
        let itm = itm.expect("found");
        self.data.table_cache.remove(&TableCacheKey {
            table_id: itm.table_id,
            schema_version: self.schema_meta_version(),
        });
        self.data.table_cache.remove(&TableCacheKey {
            table_id: itm.table_id,
            schema_version: itm.schema_version,
        });
    }
}

/// IsSpecialDB tells whether the database is a special database.
// go: IsSpecialDB L1
pub fn is_special_db(db_name: &str) -> bool {
    db_name == metadef::InformationSchemaName.l
        || db_name == metadef::PerformanceSchemaName.l
        || db_name == metadef::MetricSchemaName.l
}

struct TableByNameHelper {
    end: TableItem,
    schema_version: i64,
    found: bool,
    res: Option<TableItem>,
}

impl TableByNameHelper {
    // go: tableByNameHelper.onItem L1
    fn on_item(&mut self, item: &TableItem) -> bool {
        if item.db_name.l != self.end.db_name.l || item.table_name.l != self.end.table_name.l {
            self.found = false;
            return false;
        }
        if item.schema_version <= self.schema_version {
            if !item.tomb {
                // If the item is a tomb record, the database is dropped.
                self.found = true;
                self.res = Some(item.clone());
            }
            return false;
        }
        true
    }
}

impl InfoSchemaV2 {
    /// TableByName implements the InfoSchema interface.
    /// When schema cache miss, it will fetch the TableInfo from TikV and refill cache.
    /// Go observes `TableByNameHitDuration` / `TableByNameMissDuration` (pkg/metrics, not
    /// ported).
    // go: infoschemaV2.TableByName L1
    pub async fn table_by_name(
        &self,
        ctx: Ctx,
        schema: &CIStr,
        tbl: &CIStr,
    ) -> Result<TableRef, Error> {
        if is_special_db(&schema.l) {
            if let Some(tb_names) = self.data.specials.read().get(&schema.l)
                && let Some(t) = tb_names.tables.get(&tbl.l)
            {
                return Ok(t.clone());
            }
            return Err(ErrTableNotExists.fast_gen_by_args(vec![cistr_arg(schema), cistr_arg(tbl)]));
        }

        self.keep_alive();
        let mut h = TableByNameHelper {
            end: TableItem {
                db_name: schema.clone(),
                db_id: 0,
                table_name: tbl.clone(),
                table_id: 0,
                schema_version: i64::MAX,
                tomb: false,
            },
            schema_version: self.schema_meta_version(),
            found: false,
            res: None,
        };
        {
            let bt = self.data.by_name.read();
            for item in bt.range(..=by_name_key(&h.end)).rev().map(|(_, v)| v) {
                if !h.on_item(item) {
                    break;
                }
            }
        }

        if !h.found {
            return Err(ErrTableNotExists.fast_gen_by_args(vec![cistr_arg(schema), cistr_arg(tbl)]));
        }
        let itm = h.res.expect("found");

        // Get from the cache with old key
        let old_key = TableCacheKey {
            table_id: itm.table_id,
            schema_version: itm.schema_version,
        };
        if let Some(res) = self.data.table_cache.get(&old_key) {
            return Ok(res);
        }

        // Maybe the table is evicted? need to reload.
        let ret = self
            .load_table_info(
                ctx,
                itm.table_id,
                itm.db_id,
                self.ts,
                self.schema_meta_version(),
            )
            .await?;

        let refill = ctx.refill.unwrap_or(true);
        if refill {
            self.data.table_cache.set(old_key, ret.clone());
        }

        Ok(ret)
    }

    /// TableInfoByName implements InfoSchema.TableInfoByName
    // go: infoschemaV2.TableInfoByName L1
    pub async fn table_info_by_name(
        &self,
        schema: &CIStr,
        table: &CIStr,
    ) -> (Option<Arc<TableInfo>>, Result<(), Error>) {
        match self.table_by_name(Ctx::background(), schema, table).await {
            Ok(t) => (get_table_info(Some(&t)), Ok(())),
            Err(e) => (None, Err(e)),
        }
    }

    /// TableInfoByID implements InfoSchema.TableInfoByID
    // go: infoschemaV2.TableInfoByID L1
    pub async fn table_info_by_id(&self, id: i64) -> Option<Arc<TableInfo>> {
        let tbl = self.table_by_id(Ctx::background(), id).await;
        get_table_info(tbl.as_ref())
    }

    /// keepAlive prevents the "GC life time is shorter than transaction duration" error on
    /// infoschema v2. It works by collecting the min TS of the during infoschem v2 API
    /// calls, and reports the min TS to info.InfoSyncer.
    // go: infoschemaV2.keepAlive L1
    fn keep_alive(&self) {
        loop {
            let v = self.data.recent_min_ts.load(Ordering::SeqCst);
            if v <= self.ts {
                break;
            }
            if self
                .data
                .recent_min_ts
                .compare_exchange(v, self.ts, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
    }

    /// The meta reader of a v2 lookup at the InfoSchema's timestamp.
    // go-part: infoschemaV2.SchemaTableInfos
    fn snapshot_reader(&self) -> Mutator {
        let r = self
            .r
            .as_ref()
            .expect("infoschema v2 needs an autoid.Requirement to read meta");
        let snapshot = r.store().get_snapshot(new_version(self.ts));
        // Using the KV timeout read feature to address the issue of potential DDL lease expiration when
        // the meta region leader is slow.
        snapshot.set_option(TiKVClientReadTimeout, Box::new(3000u64)); // 3000ms.
        meta::new_reader(snapshot)
    }

    /// SchemaTableInfos implements MetaOnlyInfoSchema. `None` for Go's nil slice.
    // go: infoschemaV2.SchemaTableInfos L1 [M-GOCTX]
    pub async fn schema_table_infos(
        &self,
        _ctx: Ctx,
        schema: &CIStr,
    ) -> Result<Option<Vec<Arc<TableInfo>>>, Error> {
        if is_special_db(&schema.l) {
            if let Some(sch_tbls) = self.data.specials.read().get(&schema.l) {
                let tables: Vec<TableRef> = sch_tbls.tables.values().cloned().collect();
                return Ok(get_table_info_list(Some(&tables)));
            }
            return Ok(None); // something wrong?
        }

        self.keep_alive();
        loop {
            let Some(db_info) = self.schema_by_name(schema) else {
                return Ok(None);
            };
            let m = self.snapshot_reader();
            match m.list_tables(db_info.id).await {
                Ok(tbl_infos) => return Ok(Some(tbl_infos.into_iter().map(Arc::new).collect())),
                Err(err) => {
                    if ErrDBNotExists.equal(&err) {
                        return Ok(None);
                    }
                    // Flashback statement could cause such kind of error.
                    // In theory that error should be handled in the lower layer, like client-go.
                    // But it's not done, so we retry here.
                    if String::from_utf8_lossy(&err.error()).contains("in flashback progress") {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }
    }

    /// SchemaSimpleTableInfos implements MetaOnlyInfoSchema. `None` for Go's nil slice.
    // go: infoschemaV2.SchemaSimpleTableInfos L1
    pub fn schema_simple_table_infos(&self, schema: &CIStr) -> Option<Vec<TableNameInfo>> {
        if is_special_db(&schema.l) {
            if let Some(sch_tbls) = self.data.specials.read().get(&schema.l) {
                let mut ret = Vec::with_capacity(sch_tbls.tables.len());
                for tbl in sch_tbls.tables.values() {
                    ret.push(TableNameInfo {
                        id: tbl.meta().id,
                        name: tbl.meta().name.clone(),
                    });
                }
                return Some(ret);
            }
            return None; // something wrong?
        }

        // Ascend is much more difficult than Descend.
        // So the data is taken out first and then dedup in Descend order.
        let mut table_items: Vec<TableItem> = Vec::new();
        {
            let bt = self.data.by_name.read();
            let start = (schema.l.clone(), String::new(), 0i64);
            for item in bt
                .range((Bound::Included(start), Bound::Unbounded))
                .map(|(_, v)| v)
            {
                if item.db_name.l != schema.l {
                    break;
                }
                if self.schema_meta_version() >= item.schema_version {
                    table_items.push(item.clone());
                }
            }
        }
        if table_items.is_empty() {
            return None;
        }
        let mut tbl_infos = Vec::with_capacity(table_items.len());
        let mut curr: Option<&TableItem> = None;
        for item in table_items.iter().rev() {
            if curr.is_none_or(|c| c.table_name != item.table_name) {
                curr = Some(item);
                if !item.tomb {
                    tbl_infos.push(TableNameInfo {
                        id: item.table_id,
                        name: item.table_name.clone(),
                    });
                }
            }
        }
        Some(tbl_infos)
    }

    /// FindTableInfoByPartitionID implements InfoSchema.FindTableInfoByPartitionID
    // go: infoschemaV2.FindTableInfoByPartitionID L1
    pub async fn find_table_info_by_partition_id(
        &self,
        partition_id: i64,
    ) -> (
        Option<Arc<TableInfo>>,
        Option<Arc<DBInfo>>,
        Option<PartitionDefinition>,
    ) {
        let (tbl, db, part_def) = self.find_table_by_partition_id(partition_id).await;
        (get_table_info(tbl.as_ref()), db, part_def)
    }

    // go: infoschemaV2.SchemaByName L1
    pub fn schema_by_name(&self, schema: &CIStr) -> Option<Arc<DBInfo>> {
        if is_special_db(&schema.l) {
            return self
                .data
                .specials
                .read()
                .get(&schema.l)
                .map(|s| s.db_info.clone());
        }

        let mut val = None;
        let bt = self.data.schema_map.read();
        for item in bt
            .range(..=(schema.l.clone(), i64::MAX))
            .rev()
            .map(|(_, v)| v)
        {
            if item.name() != schema.l {
                break;
            }
            if item.schema_version <= self.schema_meta_version() {
                if !item.tomb {
                    // If the item is a tomb record, the database is dropped.
                    val = Some(item.db_info.clone());
                }
                break;
            }
        }
        val
    }

    // go: infoschemaV2.allSchemas L1 [M-MAPORDER]
    fn all_schemas_visit(&self, mut visit: impl FnMut(&Arc<DBInfo>)) {
        {
            let bt = self.data.schema_map.read();
            let mut last: Option<&Arc<DBInfo>> = None;
            for item in bt.values().rev() {
                if item.schema_version > self.schema_meta_version() {
                    // Skip the versions that we are not looking for.
                    continue;
                }

                // Dedup the same db record of different versions.
                if last.is_some_and(|l| l.name == item.db_info.name) {
                    continue;
                }
                last = Some(&item.db_info);

                if !item.tomb {
                    visit(&item.db_info);
                }
            }
        }
        for sc in self.data.specials.read().values() {
            visit(&sc.db_info);
        }
    }

    // go: infoschemaV2.AllSchemas L1
    pub fn all_schemas(&self) -> Vec<Arc<DBInfo>> {
        let mut schemas = Vec::new();
        self.all_schemas_visit(|di| schemas.push(di.clone()));
        schemas
    }

    // go: infoschemaV2.AllSchemaNames L1
    pub fn all_schema_names(&self) -> Vec<CIStr> {
        let mut rs = Vec::with_capacity(self.data.schema_map.read().len());
        self.all_schemas_visit(|di| rs.push(di.name.clone()));
        rs
    }

    // go: infoschemaV2.SchemaExists L1
    pub fn schema_exists(&self, schema: &CIStr) -> bool {
        self.schema_by_name(schema).is_some()
    }

    // go: infoschemaV2.searchPartitionItemByPartitionID L1
    fn search_partition_item_by_partition_id(&self, partition_id: i64) -> (PartitionItem, bool) {
        let mut pi = PartitionItem {
            partition_id: 0,
            schema_version: 0,
            table_id: 0,
            tomb: false,
        };
        let mut ok = false;
        let bt = self.data.pid2tid.read();
        for item in bt.range(..=(partition_id, i64::MAX)).rev().map(|(_, v)| v) {
            if item.partition_id != partition_id {
                break;
            }
            if item.schema_version > self.schema_meta_version() {
                // Skip the record.
                continue;
            }
            if item.schema_version <= self.schema_meta_version() {
                pi = *item;
                ok = !item.tomb;
                break;
            }
        }
        (pi, ok)
    }

    /// TableItemByPartitionID implements InfoSchema.TableItemByPartitionID.
    /// It returns the lightweight meta info, no worries about access the storage.
    // go: infoschemaV2.TableItemByPartitionID L1
    pub fn table_item_by_partition_id(&self, partition_id: i64) -> Option<TableItemOut> {
        let (pi, ok) = self.search_partition_item_by_partition_id(partition_id);
        if !ok {
            return None;
        }
        self.table_item_by_id(pi.table_id)
    }

    /// TableIDByPartitionID implements InfoSchema.TableIDByPartitionID.
    // go: infoschemaV2.TableIDByPartitionID L1
    pub fn table_id_by_partition_id(&self, partition_id: i64) -> Option<i64> {
        let (pi, ok) = self.search_partition_item_by_partition_id(partition_id);
        if !ok {
            return None;
        }
        Some(pi.table_id)
    }

    // go: infoschemaV2.FindTableByPartitionID L1
    pub async fn find_table_by_partition_id(
        &self,
        partition_id: i64,
    ) -> (
        Option<TableRef>,
        Option<Arc<DBInfo>>,
        Option<PartitionDefinition>,
    ) {
        let (pi, ok) = self.search_partition_item_by_partition_id(partition_id);
        if !ok {
            return (None, None, None);
        }

        let Some(tbl) = self.table_by_id(Ctx::background(), pi.table_id).await else {
            // something wrong?
            return (None, None, None);
        };

        let db_id = tbl.meta().dbid;
        let Some(db_info) = self.schema_by_id(db_id) else {
            // something wrong?
            return (None, None, None);
        };

        // Go dereferences GetPartitionInfo() without a nil check: a table that is not
        // partitioned (any more) panics.
        let part_info = tbl.meta().get_partition_info().unwrap_or_else(|| {
            panic!("runtime error: invalid memory address or nil pointer dereference")
        });
        let mut def = None;
        for pdef in part_info.definitions.iter().flatten() {
            if pdef.id == partition_id {
                def = Some(pdef.clone());
                break;
            }
        }

        (Some(tbl), Some(db_info), def)
    }

    // go: infoschemaV2.TableExists L1
    pub async fn table_exists(&self, schema: &CIStr, table: &CIStr) -> bool {
        self.table_by_name(Ctx::background(), schema, table)
            .await
            .is_ok()
    }

    // go: infoschemaV2.SchemaByID L1 [M-MAPORDER]
    pub fn schema_by_id(&self, id: i64) -> Option<Arc<DBInfo>> {
        if autoid::is_mem_schema_id(id) {
            return self
                .data
                .specials
                .read()
                .values()
                .find(|tmp| tmp.db_info.id == id)
                .map(|st| st.db_info.clone());
        }
        let mut ok = false;
        let mut name = CIStr::default();
        {
            let bt = self.data.schema_id2_name.read();
            for item in bt.range(..=(id, i64::MAX)).rev().map(|(_, v)| v) {
                if item.id != id {
                    ok = false;
                    break;
                }
                if item.schema_version <= self.schema_meta_version() {
                    if !item.tomb {
                        // If the item is a tomb record, the database is dropped.
                        ok = true;
                        name = item.name.clone();
                    }
                    break;
                }
            }
        }
        if !ok {
            return None;
        }
        self.schema_by_name(&name)
    }

    /// GetTableReferredForeignKeys implements InfoSchema.GetTableReferredForeignKeys
    // go: infoschemaV2.GetTableReferredForeignKeys L1
    pub fn get_table_referred_foreign_keys(
        &self,
        schema: &str,
        table: &str,
    ) -> Vec<Arc<ReferredFKInfo>> {
        self.keep_alive();
        self.data
            .get_table_referred_foreign_keys(schema, table, self.schema_meta_version())
    }

    /// Go's failpoint `mockLoadTableInfoError` and tracing region are not ported.
    // go: infoschemaV2.loadTableInfo L1 [M-GOCTX]
    async fn load_table_info(
        &self,
        _ctx: Ctx,
        tbl_id: i64,
        db_id: i64,
        ts: u64,
        schema_version: i64,
    ) -> Result<TableRef, Error> {
        // Try to avoid repeated concurrency loading.
        let key = format!("{db_id}-{tbl_id}-{schema_version}");
        let r = self.r.clone();
        let factory = self.factory.clone();
        let fut = {
            let mut sf = LOAD_TABLE_SF.lock();
            sf.entry(key.clone())
                .or_insert_with(|| {
                    load_table_info_once(r, factory, tbl_id, db_id, ts)
                        .boxed()
                        .shared()
                })
                .clone()
        };
        // The guard removes the entry once the load has finished, or when its last waiter
        // is dropped before that (the load is cancelled with it).
        let guard = LoadTableSfGuard { key, fut };
        let res = guard.fut.clone().await;
        drop(guard);

        match res? {
            Some(t) => Ok(t),
            None => Err(ErrTableNotExists.fast_gen_by_args(vec![
                Arg::from(format!("(Schema ID {db_id})")),
                Arg::from(format!("(Table ID {tbl_id})")),
            ])),
        }
    }
}

/// Removes a `loadTableSF` entry when a waiter leaves: always once the load is done (Go:
/// `Do` deletes the key after the function returns), and when the last waiter is dropped
/// before that (Go's goroutine would finish the load alone; here the load is cancelled with
/// its last waiter, so a later call starts a new one instead of joining a dead future).
struct LoadTableSfGuard {
    key: String,
    fut: Shared<BoxFuture<'static, LoadResult>>,
}

impl Drop for LoadTableSfGuard {
    // glue: singleflight.Group's key removal under cancellation (M-GOCTX)
    fn drop(&mut self) {
        let mut sf = LOAD_TABLE_SF.lock();
        let ours = sf.get(&self.key).is_some_and(|f| f.ptr_eq(&self.fut));
        // The map holds one clone, this guard another: 2 means no other waiter.
        let last = self.fut.strong_count().is_none_or(|n| n <= 2);
        if ours && (self.fut.peek().is_some() || last) {
            sf.remove(&self.key);
        }
    }
}

/// The function `loadTableSF.Do` runs.
// go-part: infoschemaV2.loadTableInfo
async fn load_table_info_once(
    r: Option<Arc<dyn Requirement>>,
    factory: Option<Factory>,
    tbl_id: i64,
    db_id: i64,
    ts: u64,
) -> LoadResult {
    let r = r.expect("infoschema v2 needs an autoid.Requirement to read meta");
    loop {
        let snapshot = r.store().get_snapshot(new_version(ts));
        // Using the KV timeout read feature to address the issue of potential DDL lease expiration when
        // the meta region leader is slow.
        snapshot.set_option(TiKVClientReadTimeout, Box::new(3000u64)); // 3000ms.
        let m = meta::new_reader(snapshot);

        let (tbl_info, err) = m.get_table(db_id, tbl_id).await;
        if let Err(err) = err {
            // Flashback statement could cause such kind of error.
            // In theory that error should be handled in the lower layer, like client-go.
            // But it's not done, so we retry here.
            if String::from_utf8_lossy(&err.error()).contains("in flashback progress") {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            return Err(err);
        }

        // table removed.
        let Some(mut tbl_info) = tbl_info else {
            return Err(ErrTableNotExists.fast_gen_by_args(vec![
                Arg::from(format!("(Schema ID {db_id})")),
                Arg::from(format!("(Table ID {tbl_id})")),
            ]));
        };

        convert_charset_collate_to_lower_case_if_need(&mut tbl_info);
        convert_old_version_utf8_to_utf8mb4_if_need(&mut tbl_info);
        let allocs = autoid::new_allocators_from_tbl_info(Some(&r), db_id, &tbl_info);
        let ret = table_from_meta(allocs, factory.as_ref(), Arc::new(tbl_info))?;
        return Ok(Some(ret));
    }
}

/// A `model.CIStr` argument (its `String()` is `O`).
// glue: Go fmt argument of type model.CIStr
pub(crate) fn cistr_arg(s: &CIStr) -> Arg {
    Arg::Stringer(s.o.clone().into_bytes(), "model.CIStr")
}

/// IsV2 tells whether an InfoSchema is v2 or not. tidb-rs has only v2.
// go: IsV2 L2 (only v2 exists: always true)
pub fn is_v2(is: &InfoSchemaV2) -> (bool, &InfoSchemaV2) {
    (true, is)
}

// go: applyTableUpdate L1
pub(crate) async fn apply_table_update(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    b.apply_table_update_v2(m, diff).await
}

// go: applyCreateSchema L1
pub(crate) async fn apply_create_schema(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<(), Error> {
    b.apply_create_schema(m, diff).await
}

// go: applyDropSchema L1
pub(crate) async fn apply_drop_schema(b: &mut Builder, diff: &SchemaDiff) -> Vec<i64> {
    b.apply_drop_schema_v2(diff).await
}

/// `ReadTableFromMeta` (BR's recover schema) lists the tables with
/// `meta.ListSimpleTables`, which is D06.2 part 2.
// go: applyRecoverSchema L1
pub(crate) async fn apply_recover_schema(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    if diff.read_table_from_meta {
        // recover tables under the database and set them to diff.AffectedOpts
        // glue: stub, blocked-by:D06.2 (part 2: Mutator.ListSimpleTables)
        return Err(tidb_base::errors::new(
            "infoschema: recover schema with ReadTableFromMeta is not supported yet (blocked-by:D06.2 part 2, Mutator.ListSimpleTables)",
        ));
    }

    b.apply_recover_schema_v2(m, diff).await
}

impl Builder {
    // go: Builder.applyRecoverSchemaV2 L1
    async fn apply_recover_schema_v2(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<Vec<i64>, Error> {
        if let Some(di) = self.is.schema_by_id(diff.schema_id) {
            return Err(ErrDatabaseExists
                .gen_with_stack_by_args(vec![Arg::from(format!("(Schema ID {})", di.id))]));
        }
        let (di, err) = m.get_database(diff.schema_id).await;
        err?;
        // Go passes a nil *DBInfo on to addDB, which dereferences it.
        let di = di.unwrap_or_else(|| {
            panic!("runtime error: invalid memory address or nil pointer dereference")
        });
        self.info_data.add_db(diff.version, di);
        apply_create_tables(self, m, diff).await
    }
}

// go: applyModifySchemaCharsetAndCollate L1
pub(crate) async fn apply_modify_schema_charset_and_collate(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<(), Error> {
    b.apply_modify_schema_charset_and_collate_v2(m, diff).await
}

// go: applyModifySchemaDefaultPlacement L1
pub(crate) async fn apply_modify_schema_default_placement(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<(), Error> {
    b.apply_modify_schema_default_placement_v2(m, diff).await
}

// go: applyDropTable L1
pub(crate) async fn apply_drop_table(
    b: &mut Builder,
    diff: &SchemaDiff,
    db_info: &DBInfo,
    table_id: i64,
    affected: Vec<i64>,
) -> Vec<i64> {
    b.apply_drop_table_v2(diff, db_info, table_id, affected)
        .await
}

// go: applyCreateTables L1
pub(crate) async fn apply_create_tables(
    b: &mut Builder,
    m: &Mutator,
    diff: &SchemaDiff,
) -> Result<Vec<i64>, Error> {
    b.apply_create_tables(m, diff).await
}

// go: updateInfoSchemaBundles L1
pub(crate) async fn update_info_schema_bundles(b: &mut Builder) {
    let is = b.is.clone();
    let misc = Arc::make_mut(&mut b.is.misc);
    b.bundle.update_info_schema_bundles_v2(&is, misc).await;
}

// go: oldSchemaInfo L1
pub(crate) fn old_schema_info(b: &Builder, diff: &SchemaDiff) -> Option<Arc<DBInfo>> {
    b.is.schema_by_id(diff.old_schema_id)
}

/// allocByID returns the Allocators of a table.
// go: allocByID L1
pub(crate) async fn alloc_by_id(b: &Builder, id: i64) -> (autoid::Allocators, bool) {
    match b.is.table_by_id(Ctx::background(), id).await {
        Some(tbl) => (tbl.allocators(), true),
        None => (autoid::Allocators::default(), false),
    }
}

impl Builder {
    // TODO: more UT to check the correctness.
    // go: Builder.applyTableUpdateV2 L1
    pub(crate) async fn apply_table_update_v2(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<Vec<i64>, Error> {
        let Some(old_db_info) = self.is.schema_by_id(diff.schema_id) else {
            return Err(
                ErrDatabaseNotExists.gen_with_stack_by_args(vec![Arg::from(format!(
                    "(Schema ID {})",
                    diff.schema_id
                ))]),
            );
        };

        let (old_table_id, new_table_id) = self.get_table_ids(m, diff).await?;
        self.update_bundle_for_table_update(diff, new_table_id, old_table_id);

        let (mut tbl_ids, allocs) =
            drop_table_for_update(self, new_table_id, old_table_id, &old_db_info, diff).await?;

        if table_id_is_valid(new_table_id) {
            // All types except DropTableOrView.
            tbl_ids = apply_create_table(
                self,
                m,
                &old_db_info,
                new_table_id,
                allocs,
                diff.type_,
                tbl_ids,
                diff.version,
            )
            .await?;
        }
        Ok(tbl_ids)
    }

    // go: Builder.applyDropSchemaV2 L1
    pub(crate) async fn apply_drop_schema_v2(&mut self, diff: &SchemaDiff) -> Vec<i64> {
        let Some(di) = self.is.schema_by_id(diff.schema_id) else {
            return Vec::new();
        };

        let mut table_ids = Vec::with_capacity(di.deprecated.tables.as_ref().map_or(0, Vec::len));
        let tables = match self
            .is
            .schema_table_infos(Ctx::background(), &di.name)
            .await
        {
            Ok(t) => t,
            Err(err) => {
                terror::log(Some(&err));
                None
            }
        };
        for tbl in tables.iter().flatten() {
            table_ids = append_affected_ids(table_ids, tbl);
        }

        for &id in &table_ids {
            let misc = Arc::make_mut(&mut self.is.misc);
            self.bundle.delete_bundle(misc, id);
            self.apply_drop_table_v2(diff, &di, id, Vec::new()).await;
        }
        self.info_data.delete_db(di, diff.version);
        table_ids
    }

    /// Go returns nil when the table is not found (not `affected`).
    // go: Builder.applyDropTableV2 L1
    pub(crate) async fn apply_drop_table_v2(
        &mut self,
        diff: &SchemaDiff,
        db_info: &DBInfo,
        table_id: i64,
        mut affected: Vec<i64>,
    ) -> Vec<i64> {
        // Remove the table in temporaryTables
        if let Some(ids) = &mut Arc::make_mut(&mut self.is.misc).temporary_table_ids {
            ids.remove(&table_id);
        }

        let Some(table) = self.is.table_by_id(Ctx::background(), table_id).await else {
            return Vec::new();
        };
        let tbl_info = table.meta();

        // The old DBInfo still holds a reference to old table info, we need to remove it.
        // (Go also calls b.infoSchema.deleteReferredForeignKeys: the v1 map, empty under v2.)
        self.is
            .data
            .delete_referred_foreign_keys(&db_info.name, tbl_info, diff.version);

        if let Some(pi) = table.meta().get_partition_info() {
            for def in pi.definitions.iter().flatten() {
                let p = PartitionItem {
                    partition_id: def.id,
                    schema_version: diff.version,
                    table_id: table.meta().id,
                    tomb: true,
                };
                btree_set(&self.info_data.pid2tid, partition_item_key(&p), p);
            }
        }

        self.info_data.remove(TableItem {
            db_name: db_info.name.clone(),
            db_id: db_info.id,
            table_name: tbl_info.name.clone(),
            table_id: tbl_info.id,
            schema_version: diff.version,
            tomb: false,
        });
        affected = append_affected_ids(affected, tbl_info);

        affected
    }

    /// Go writes the new charset into the `*DBInfo` it got from `SchemaByID`, which the
    /// older schema items share; [`Data::replace_shared_db_info`] gives the same lookups.
    /// A nil DBInfo panics as in Go.
    // go: Builder.applyModifySchemaCharsetAndCollateV2 L2 (Go mutates the shared *DBInfo; Rust replaces it in the items that share it)
    pub(crate) async fn apply_modify_schema_charset_and_collate_v2(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<(), Error> {
        let (di, err) = m.get_database(diff.schema_id).await;
        err?;
        let Some(di) = di else {
            // This should never happen.
            return Err(
                ErrDatabaseNotExists.gen_with_stack_by_args(vec![Arg::from(format!(
                    "(Schema ID {})",
                    diff.schema_id
                ))]),
            );
        };
        let old = self.is.schema_by_id(diff.schema_id).unwrap_or_else(|| {
            panic!("runtime error: invalid memory address or nil pointer dereference")
        });
        let mut new_db_info = old.copy();
        new_db_info.charset = di.charset.clone();
        new_db_info.collate = di.collate.clone();
        let new_db_info = Arc::new(new_db_info);
        self.info_data.replace_shared_db_info(&old, &new_db_info);
        self.info_data.delete_db(Arc::new(di), diff.version);
        self.info_data.add_db_arc(diff.version, new_db_info);
        Ok(())
    }

    // go: Builder.applyModifySchemaDefaultPlacementV2 L2 (Go mutates the shared *DBInfo; Rust replaces it in the items that share it)
    pub(crate) async fn apply_modify_schema_default_placement_v2(
        &mut self,
        m: &Mutator,
        diff: &SchemaDiff,
    ) -> Result<(), Error> {
        let (di, err) = m.get_database(diff.schema_id).await;
        err?;
        let Some(di) = di else {
            // This should never happen.
            return Err(
                ErrDatabaseNotExists.gen_with_stack_by_args(vec![Arg::from(format!(
                    "(Schema ID {})",
                    diff.schema_id
                ))]),
            );
        };
        let old = self.is.schema_by_id(diff.schema_id).unwrap_or_else(|| {
            panic!("runtime error: invalid memory address or nil pointer dereference")
        });
        let mut new_db_info = old.copy();
        new_db_info.placement_policy_ref = di.placement_policy_ref.clone();
        let new_db_info = Arc::new(new_db_info);
        self.info_data.replace_shared_db_info(&old, &new_db_info);
        self.info_data.delete_db(Arc::new(di), diff.version);
        self.info_data.add_db_arc(diff.version, new_db_info);
        Ok(())
    }
}

impl BundleInfoBuilder {
    // go: bundleInfoBuilder.updateInfoSchemaBundlesV2 L1 [M-MAPORDER]
    pub(crate) async fn update_info_schema_bundles_v2(
        &mut self,
        is: &InfoSchemaV2,
        misc: &mut InfoSchemaMisc,
    ) {
        if self.delta_update {
            self.complete_update_tables_v2(is);
            let ids: Vec<i64> = self.update_tables.keys().copied().collect();
            for tbl_id in ids {
                self.update_table_bundles(is, misc, tbl_id).await;
            }
            return;
        }

        // do full update bundles
        misc.rule_bundle_map = HashMap::new();
        let tmp =
            is.list_tables_with_special_attribute(infoschemacontext::placement_policy_attribute);
        for v in tmp {
            for tbl in &v.table_infos {
                self.update_table_bundles(is, misc, tbl.id).await;
            }
        }
    }

    // go: bundleInfoBuilder.completeUpdateTablesV2 L1
    fn complete_update_tables_v2(&mut self, is: &InfoSchemaV2) {
        if self.update_policies.is_empty() {
            return;
        }

        let dbs = is.list_tables_with_special_attribute(infoschemacontext::AllSpecialAttribute);
        for db in dbs {
            for tbl in &db.table_infos {
                let tbl_info = tbl;
                if let Some(r) = &tbl_info.placement_policy_ref
                    && self.update_policies.contains_key(&r.id)
                {
                    self.mark_table_bundle_should_update(tbl_info.id);
                }
            }
        }
    }
}

impl InfoSchemaV2 {
    // go: infoschemaV2.ListTablesWithSpecialAttribute L1
    pub fn list_tables_with_special_attribute(
        &self,
        filter: SpecialAttributeFilter,
    ) -> Vec<TableInfoResult> {
        let mut ret = Vec::with_capacity(10);
        let mut curr_db = String::new();
        let mut last_table_id = 0i64;
        let mut res = TableInfoResult::default();
        let bt = self.data.table_info_resident.read();
        for item in bt.values().rev() {
            if item.schema_version > self.schema_meta_version() {
                // Skip the versions that we are not looking for.
                continue;
            }
            // Dedup the same record of different versions.
            if last_table_id != 0 && last_table_id == item.table_id {
                continue;
            }
            last_table_id = item.table_id;

            if item.tomb {
                continue;
            }

            let table_info = item
                .table_info
                .as_ref()
                .expect("a resident item that is not a tomb has its table info");
            if !filter(table_info) {
                continue;
            }

            if curr_db.is_empty() {
                curr_db = item.db_name.l.clone();
                res = TableInfoResult {
                    db_name: item.db_name.clone(),
                    table_infos: Vec::new(),
                };
                res.table_infos.push(table_info.clone());
            } else if curr_db == item.db_name.l {
                res.table_infos.push(table_info.clone());
            } else {
                ret.push(std::mem::take(&mut res));
                res = TableInfoResult {
                    db_name: item.db_name.clone(),
                    table_infos: Vec::new(),
                };
                res.table_infos.push(table_info.clone());
            }
        }
        if !res.table_infos.is_empty() {
            ret.push(res);
        }
        ret
    }
}

/// WithRefillOption controls the infoschema v2 cache refill operation.
/// By default, TableByID does not refill schema cache, and TableByName does.
/// The behavior can be changed by providing the context.Context.
// go: WithRefillOption L1 [M-GOCTX]
pub fn with_refill_option(mut ctx: Ctx, refill: bool) -> Ctx {
    ctx.refill = Some(refill);
    ctx
}

#[cfg(test)]
mod tests {
    //! go-file: pkg/infoschema/infoschema_v2_test.go (the tests on `Data` alone)
    use super::*;
    use tidb_model::FKInfo;

    fn ci(s: &str) -> CIStr {
        CIStr::new(s)
    }

    fn rfk(cs: &str, ct: &str, cf: &str) -> ReferredFKInfo {
        ReferredFKInfo {
            cols: None,
            child_schema: ci(cs),
            child_table: ci(ct),
            child_fk_name: ci(cf),
        }
    }

    fn got(v: Vec<Arc<ReferredFKInfo>>) -> Vec<ReferredFKInfo> {
        v.iter().map(|r| (**r).clone()).collect()
    }

    fn fk(name: &str, ref_schema: &str, ref_table: &str) -> FKInfo {
        FKInfo {
            name: ci(name),
            ref_schema: ci(ref_schema),
            ref_table: ci(ref_table),
            version: FKVersion1,
            ..Default::default()
        }
    }

    #[test]
    fn test_referred_foreign_keys() {
        let data = new_data();

        // schema1.table1 with two FKs
        let tbl1 = TableInfo {
            name: ci("table1"),
            foreign_keys: Some(vec![fk("fk1", "db1", "table1"), fk("fk2", "db1", "table1")]),
            ..Default::default()
        };
        data.add_referred_foreign_keys(&ci("db1"), &tbl1, 1);
        let expected1 = vec![rfk("db1", "table1", "fk1"), rfk("db1", "table1", "fk2")];
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db1", "table1", 1)),
            expected1
        );

        // schema1.table2 has none
        assert!(
            data.get_table_referred_foreign_keys("db1", "table2", 1)
                .is_empty()
        );

        // schema2.tableA with one FK
        let tbl_a = TableInfo {
            name: ci("tableA"),
            foreign_keys: Some(vec![fk("fkA", "db2", "tableA")]),
            ..Default::default()
        };
        data.add_referred_foreign_keys(&ci("db2"), &tbl_a, 2);
        let expected2 = vec![rfk("db2", "tableA", "fkA")];
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db2", "tablea", 2)),
            expected2
        );

        // cross-schema/table lookups return empty
        assert!(
            data.get_table_referred_foreign_keys("db1", "tablea", 2)
                .is_empty()
        );
        assert!(
            data.get_table_referred_foreign_keys("db2", "table1", 2)
                .is_empty()
        );

        // delete all FKs for db1.table1
        data.delete_referred_foreign_keys(&ci("db1"), &tbl1, 3);
        assert!(
            data.get_table_referred_foreign_keys("db1", "table1", 3)
                .is_empty()
        );
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db2", "tablea", 3)),
            expected2
        );

        // delete the FK for db2.tableA
        data.delete_referred_foreign_keys(&ci("db2"), &tbl_a, 4);
        assert!(
            data.get_table_referred_foreign_keys("db2", "tablea", 4)
                .is_empty()
        );

        // get with old version
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db1", "table1", 2)),
            expected1
        );
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db2", "tablea", 2)),
            expected2
        );
    }

    #[test]
    fn test_gc_old_fk_version() {
        let data = new_data();
        // helper to make items
        let mk = |db: &str, tbl: &str, ver: i64, tomb: bool| ReferredForeignKeyItem {
            db_name: db.to_owned(),
            table_name: tbl.to_owned(),
            schema_version: ver,
            tomb,
            referred_fk_info: vec![Arc::new(rfk("s", "t", "fk"))],
        };
        // prepare two groups: db1.table1.fkX and db2.table2.fkY
        let items = [
            mk("db1", "table1", 5, false),
            mk("db1", "table1", 4, true),
            mk("db1", "table1", 3, false),
            mk("db1", "table1", 2, true),
            mk("db1", "table1", 1, false),
            mk("db2", "table2", 1, false), // unaffected
        ];
        for it in items {
            btree_set(
                &data.referred_foreign_keys,
                referred_foreign_key_item_key(&it),
                it,
            );
        }
        assert_eq!(data.referred_foreign_keys.read().len(), 6);

        // GC entries older than version 4
        let deleted = data.gc_old_fk_version(4);
        assert_eq!(deleted, 3); // versions 3,2,1 for group1
        // kept version 5 & 4 for group1, and the single group2
        assert_eq!(data.referred_foreign_keys.read().len(), 3);

        // verify surviving versions
        let vers: Vec<i64> = data
            .referred_foreign_keys
            .read()
            .values()
            .map(|i| i.schema_version)
            .collect();
        assert_eq!(vers, [4, 5, 1]);

        // ensure getTableReferredForeignKeys respects GC boundary
        let g = data.get_table_referred_foreign_keys("db1", "table1", 5);
        assert_eq!(*g[0], rfk("s", "t", "fk"));
    }

    // ------------------------------------------------------------ singleflight guard

    use std::collections::HashMap;
    use std::future::Future;
    use std::task::{Context as TaskContext, Poll};

    use tidb_kv::Context as KvContext;
    use tidb_kv::key::Key;
    use tidb_kv::kv::{
        BatchGetOption, BatchGetter, Client, GetOption, Getter, Iterator as KvIterator, KvFuture,
        OptionValue, Retriever, Snapshot, Storage, Transaction, TxnOption, ValueEntry,
    };
    use tidb_kv::version::Version;

    /// A snapshot whose reads never finish.
    struct PendingSnap;

    impl Getter for PendingSnap {
        fn get<'a>(
            &'a self,
            _: &'a KvContext,
            _: &'a Key,
            _: &'a [GetOption],
        ) -> KvFuture<'a, ValueEntry> {
            Box::pin(futures::future::pending())
        }
    }
    impl Retriever for PendingSnap {
        fn iter<'a: 'f, 'f>(
            &'a self,
            _: &'f Key,
            _: &'f Key,
        ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
            Box::pin(futures::future::pending())
        }
        fn iter_reverse<'a: 'f, 'f>(
            &'a self,
            _: &'f Key,
            _: &'f Key,
        ) -> KvFuture<'f, Box<dyn KvIterator + 'a>> {
            Box::pin(futures::future::pending())
        }
    }
    impl BatchGetter for PendingSnap {
        fn batch_get<'a>(
            &'a self,
            _: &'a KvContext,
            _: &'a [Key],
            _: &'a [BatchGetOption],
        ) -> KvFuture<'a, HashMap<Vec<u8>, ValueEntry>> {
            Box::pin(futures::future::pending())
        }
    }
    impl Snapshot for PendingSnap {
        fn set_option(&self, _: isize, _: OptionValue) {}
    }

    struct PendingStore;

    impl Storage for PendingStore {
        fn begin<'a>(&'a self, _: &'a [TxnOption]) -> KvFuture<'a, Box<dyn Transaction>> {
            unimplemented!()
        }
        fn get_snapshot(&self, _: Version) -> Arc<dyn Snapshot> {
            Arc::new(PendingSnap)
        }
        fn get_client(&self) -> Arc<dyn Client> {
            unimplemented!()
        }
        fn close(&self) -> Result<(), tidb_base::Error> {
            Ok(())
        }
        fn uuid(&self) -> String {
            String::new()
        }
        fn current_version<'a>(&'a self, _: &'a str) -> KvFuture<'a, Version> {
            unimplemented!()
        }
        fn name(&self) -> String {
            String::new()
        }
        fn describe(&self) -> String {
            String::new()
        }
        fn get_min_safe_ts(&self, _: &str) -> u64 {
            0
        }
    }

    struct PendingReq;

    impl Requirement for PendingReq {
        fn store(&self) -> Arc<dyn Storage> {
            Arc::new(PendingStore)
        }
    }

    fn poll_once<F: Future>(f: &mut std::pin::Pin<Box<F>>) -> Poll<F::Output> {
        let waker = futures::task::noop_waker();
        f.as_mut().poll(&mut TaskContext::from_waker(&waker))
    }

    /// A key's `loadTableSF` entry is removed when every waiter is dropped before the load
    /// finishes, so a later lookup starts a new load instead of joining a cancelled one.
    #[test]
    fn load_table_sf_entry_removed_when_waiters_cancelled() {
        let data = new_data();
        let db = DBInfo {
            id: 1_000_001,
            name: ci("sfdb"),
            ..Default::default()
        };
        data.add_db(1, db);
        // A name-only table (not in the cache): TableByID has to load it.
        data.add_name_only(TableItem {
            db_name: ci("sfdb"),
            db_id: 1_000_001,
            table_name: ci("sft"),
            table_id: 1_000_002,
            schema_version: 1,
            tomb: false,
        });
        let mut is = new_info_schema_v2(Some(Arc::new(PendingReq)), None, data);
        Arc::make_mut(&mut is.misc).schema_meta_version = 1;
        let key = format!("{}-{}-{}", 1_000_001, 1_000_002, 1);
        let has_key = || LOAD_TABLE_SF.lock().contains_key(&key);

        let mut a = Box::pin(is.table_by_id(Ctx::background(), 1_000_002));
        let mut b = Box::pin(is.table_by_id(Ctx::background(), 1_000_002));
        assert!(poll_once(&mut a).is_pending());
        assert!(poll_once(&mut b).is_pending());
        assert!(has_key());
        drop(a); // b still waits
        assert!(has_key());
        drop(b); // the last waiter
        assert!(!has_key());

        // A new lookup starts a new load.
        let mut c = Box::pin(is.table_by_id(Ctx::background(), 1_000_002));
        assert!(poll_once(&mut c).is_pending());
        assert!(has_key());
        drop(c);
        assert!(!has_key());
    }

    /// Deliberate deviation #195. Go's GC rule ("delete an item older than the cut if the
    /// previous, newer item is the same table") also deletes the item a reader **at** the
    /// cut needs when that newer item is above the cut: here Go deletes v3 and the reader at
    /// v5 no longer finds the table. tidb-rs keeps the newest item <= cut (v3) and deletes
    /// only what is older (v1); the same for the referred foreign keys.
    #[test]
    fn gc_keeps_the_item_a_reader_at_the_cut_needs() {
        let data = new_data();
        let item = |ver: i64| TableItem {
            db_name: ci("db"),
            db_id: 1,
            table_name: ci("t"),
            table_id: 10,
            schema_version: ver,
            tomb: false,
        };
        let tbl = crate::table::table_from_meta(
            crate::autoid::Allocators::default(),
            Arc::new(TableInfo {
                id: 10,
                name: ci("t"),
                // TableFromMeta rejects StateNone (D06.4).
                state: tidb_model::StatePublic,
                ..Default::default()
            }),
        )
        .unwrap();
        data.add(item(1), tbl.clone()); // created at v1
        data.add(item(3), tbl.clone()); // altered at v3
        data.add(item(8), tbl); // altered at v8
        let mut is5 = new_info_schema_v2(None, None, data.clone());
        Arc::make_mut(&mut is5.misc).schema_meta_version = 5;
        assert!(is5.table_item_by_id(10).is_some());
        // The oldest version in use is 5 (the reader above): only v1 goes (Go: v1 and v3).
        assert_eq!(data.gc_old_version(5).0, 1);
        assert!(is5.table_item_by_id(10).is_some());
        let vers: Vec<i64> = data
            .by_name
            .read()
            .values()
            .map(|i| i.schema_version)
            .collect();
        assert_eq!(vers, [3, 8]);

        // Referred foreign keys: v1, v3, v8 of db.p; cut 5 keeps v3 (Go deletes v1 and v3).
        let mk = |ver: i64| ReferredForeignKeyItem {
            db_name: "db".to_owned(),
            table_name: "p".to_owned(),
            schema_version: ver,
            tomb: false,
            referred_fk_info: vec![Arc::new(rfk("db", "t", &format!("fk{ver}")))],
        };
        for v in [1, 3, 8] {
            let it = mk(v);
            btree_set(
                &data.referred_foreign_keys,
                referred_foreign_key_item_key(&it),
                it,
            );
        }
        assert_eq!(data.gc_old_fk_version(5), 1);
        assert_eq!(
            got(data.get_table_referred_foreign_keys("db", "p", 5)),
            vec![rfk("db", "t", "fk3")]
        );
    }
}
