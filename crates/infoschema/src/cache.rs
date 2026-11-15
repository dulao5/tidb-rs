//! go-file: pkg/infoschema/cache.go
//!
//! `InfoCache`: the domain's recent InfoSchema versions (D06.3 part 2, T8).
//!
//! Mapping:
//! - Go's `InfoSchema` interface values are `Arc<InfoSchemaV2>` (only v2 exists, spec §1.2;
//!   N13: every version is an immutable snapshot, the versions share one [`Data`]). A version
//!   leaves the cache by being dropped from the slice; it is freed when the last statement
//!   holding it ends.
//! - `cap(h.cache)` is the field `capacity` (Go's slice capacity is the cache size).
//! - `IsV2` is always true, so `Upsert` (the v1 / v2 switch) is never called by the domain
//!   (spec §5.4); it is ported for completeness. Its "destroy the old objects" closure has
//!   nothing to do in Rust: the old versions are dropped with the slice.
//! - `gcOldVersion` needs `meta.GetOldestSchemaVersion`, which reads the MVCC history of
//!   the schema version key through `store/helper` (TiKV's MVCC debug API). That is a
//!   boundary stub (see the function); `Data` keeps the old items until it is ported.
//! - metrics (`infoschema_metrics.*Counter`) are not ported (spec §4 item 3).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::autoid::Requirement;
use crate::infoschema_v2::{Data, InfoSchemaV2, new_data};

/// One cached version and the commit ts of the schema diff that created it.
#[derive(Clone)]
struct SchemaAndTimestamp {
    infoschema: Arc<InfoSchemaV2>,
    timestamp: i64,
}

/// The fields Go guards with `mu`.
struct Inner {
    /// cache is sorted by both SchemaVersion and timestamp in descending order, assume they
    /// have same order
    cache: Vec<SchemaAndTimestamp>,
    /// `cap(cache)`.
    capacity: usize,
    /// emptySchemaVersions stores schema version which has no schema_diff.
    empty_schema_versions: HashMap<i64, ()>,
    /// first known schema version records the first known schema version, all schemas
    /// between [firstKnownSchemaVersion, latest) are known as long as we keep the DDL
    /// history correctly.
    first_known_schema_version: i64,
    last_check_version: i64,
    last_check_time: Option<Instant>,
}

/// InfoCache handles information schema, including getting and setting.
/// The cache behavior, however, is transparent and under automatic management.
/// It only promised to cache the infoschema, if it is newer than all the cached.
pub struct InfoCache {
    mu: RwLock<Inner>,
    r: Option<Arc<dyn Requirement>>,
    /// `Data`: shared by every version the domain builds.
    pub data: Arc<Data>,
}

/// NewCache creates a new InfoCache.
// go: NewCache L1
pub fn new_cache(r: Option<Arc<dyn Requirement>>, capacity: usize) -> InfoCache {
    let info_data = new_data();
    InfoCache {
        mu: RwLock::new(Inner {
            cache: Vec::with_capacity(capacity),
            capacity,
            empty_schema_versions: HashMap::new(),
            first_known_schema_version: 0,
            last_check_version: 0,
            last_check_time: None,
        }),
        r,
        data: info_data,
    }
}

/// `gcCheckInterval`.
const GC_CHECK_INTERVAL: i64 = 128;

/// `sort.Search(len(cache), func(i) bool { return cache[i].SchemaMetaVersion() <= version })`.
// glue: Go sort.Search over the descending cache
fn search_version(cache: &[SchemaAndTimestamp], version: i64) -> usize {
    cache.partition_point(|e| e.infoschema.schema_meta_version() > version)
}

impl InfoCache {
    /// GetAndResetRecentInfoSchemaTS provides the min start ts for infosync.InfoSyncer.
    /// Every round of ReportMinStartTS(), the minimal known TS used be InfoSchemaV2 APIs
    /// will be reported.
    // go: InfoCache.GetAndResetRecentInfoSchemaTS L1
    pub fn get_and_reset_recent_info_schema_ts(&self, now: u64) -> u64 {
        let ret = self.data.recent_min_ts.load(Ordering::SeqCst);
        self.data.recent_min_ts.store(now, Ordering::SeqCst);
        ret
    }

    /// ReSize re-size the cache.
    // go: InfoCache.ReSize L1
    pub fn re_size(&self, capacity: usize) {
        let mut h = self.mu.write();
        if h.capacity == capacity {
            return;
        }
        let old_cache = std::mem::take(&mut h.cache);
        h.capacity = capacity;
        h.cache = Vec::with_capacity(capacity);
        h.cache.extend(old_cache.into_iter().take(capacity));
    }

    /// Size returns the size of the cache, export for test.
    // go: InfoCache.Size L1
    pub fn size(&self) -> usize {
        self.mu.read().cache.len()
    }

    /// Reset resets the cache.
    // go: InfoCache.Reset L1
    pub fn reset(&self, capacity: usize) {
        let mut h = self.mu.write();
        h.cache = Vec::with_capacity(capacity);
        h.capacity = capacity;
    }

    /// Upsert is Resert and Insert combined, used during infoschema v1 v2 switch (never in
    /// tidb-rs, see the module doc). Go returns a function that destroys the old versions;
    /// here they are dropped with the old slice, so the returned function does nothing.
    // go: InfoCache.Upsert L2 (the old versions are dropped, not destroyed in place)
    pub fn upsert(&self, is: Arc<InfoSchemaV2>, schema_ts: u64) -> impl FnOnce() {
        let mut h = self.mu.write();
        let version = is.schema_meta_version();
        let capacity = h.capacity;
        let old = std::mem::replace(&mut h.cache, Vec::with_capacity(capacity));
        h.cache.push(SchemaAndTimestamp {
            infoschema: is,
            timestamp: schema_ts as i64,
        });
        h.first_known_schema_version = version;
        move || {
            drop(old);
            log::info!(
                "reset the old infoschema after v1 v2 switch, using the stale object will panic"
            );
        }
    }

    /// GetLatest gets the newest information schema.
    // go: InfoCache.GetLatest L1
    pub fn get_latest(&self) -> Option<Arc<InfoSchemaV2>> {
        let h = self.mu.read();
        h.cache.first().map(|e| Arc::clone(&e.infoschema))
    }

    /// Len returns the size of the cache
    // go: InfoCache.Len L1
    pub fn len(&self) -> usize {
        self.mu.read().cache.len()
    }

    /// Whether the cache is empty (clippy's companion of `len`).
    // glue: companion of Len
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// GetEmptySchemaVersions returns emptySchemaVersions, exports for testing.
    // go: InfoCache.GetEmptySchemaVersions L1
    pub fn get_empty_schema_versions(&self) -> HashMap<i64, ()> {
        self.mu.read().empty_schema_versions.clone()
    }

    // go: InfoCache.getSchemaByTimestampNoLock L1
    fn get_schema_by_timestamp_no_lock(h: &Inner, ts: u64) -> Option<Arc<InfoSchemaV2>> {
        log::debug!("SCHEMA CACHE get schema, timestamp: {ts}");
        // search one by one instead of binary search, because the timestamp of a schema could be 0
        // this is ok because the size of h.cache is small (currently set to 16)
        // moreover, the most likely hit element in the array is the first one in steady mode
        // thus it may have better performance than binary search
        for (i, is) in h.cache.iter().enumerate() {
            if is.timestamp == 0 || ts < is.timestamp as u64 {
                // is.timestamp == 0 means the schema ts is unknown, so we can't use it, then just skip it.
                // ts < is.timestamp means the schema is newer than ts, so we can't use it too, just skip it to find the older one.
                continue;
            }
            // ts >= is.timestamp must be true after the above condition.
            if i == 0 {
                // the first element is the latest schema, so we can return it directly.
                return Some(Arc::clone(&is.infoschema));
            }

            if h.cache[i - 1].timestamp as u64 > ts {
                // The first condition is to make sure the cache[i-1].timestamp > ts >= cache[i].timestamp, then the current schema is suitable for ts.
                let last_version = h.cache[i - 1].infoschema.schema_meta_version();
                let current_version = is.infoschema.schema_meta_version();
                if last_version == current_version + 1 {
                    // This condition is to make sure the schema version is continuous. If last(cache[i-1]) schema-version is 10,
                    // but current(cache[i]) schema-version is not 9, then current schema may not suitable for ts.
                    return Some(Arc::clone(&is.infoschema));
                }
                if last_version > current_version {
                    let found = (current_version + 1..last_version)
                        .all(|ver| h.empty_schema_versions.contains_key(&ver));
                    if found {
                        // This condition is to make sure the schema version is continuous. If last(cache[i-1]) schema-version is 10, and
                        // current(cache[i]) schema-version is 8, then there is a gap exist, and if all the gap version can be found in cache.emptySchemaVersions
                        // which means those gap versions don't have schema info, then current schema is also suitable for ts.
                        return Some(Arc::clone(&is.infoschema));
                    }
                }
            }
            // current schema is not suitable for ts, then break the loop to avoid the unnecessary search.
            break;
        }

        log::debug!("SCHEMA CACHE no schema found");
        None
    }

    /// GetByVersion gets the information schema based on schemaVersion. Returns nil if it
    /// is not loaded.
    // go: InfoCache.GetByVersion L1
    pub fn get_by_version(&self, version: i64) -> Option<Arc<InfoSchemaV2>> {
        let h = self.mu.read();
        Self::get_by_version_no_lock(&h, version)
    }

    // go: InfoCache.getByVersionNoLock L1
    fn get_by_version_no_lock(h: &Inner, version: i64) -> Option<Arc<InfoSchemaV2>> {
        let i = search_version(&h.cache, version);

        // `GetByVersion` is allowed to load the latest schema that is less than argument
        // `version` when the argument `version` <= the latest schema version.
        // if `version` > the latest schema version, always return nil, loadInfoSchema
        // will use this behavior to decide whether to load schema diffs or full reload.
        // Consider cache has values [10, 9, _, _, 6, 5, 4, 3, 2, 1], version 8 and 7 is empty because of the diff is empty.
        // If we want to get version 8, we can return version 6 because v7 and v8 do not change anything, they are totally the same,
        // in this case the `i` will not be 0.
        // If i == 0, it means the argument version is `10`, or greater than `10`, if `version` is 10
        // `h.cache[i].SchemaMetaVersion() == version` will be true, so we can return the latest schema, return nil if not.
        // upsert is a full reset of InfoCache, after upsert, the DDL history might lost and
        // the assumption does not hold anymore (see Go's comment).
        if i == h.cache.len() {
            return None;
        }

        let v = h.cache[i].infoschema.schema_meta_version();
        if v == version || (i != 0 && v >= h.first_known_schema_version) {
            return Some(Arc::clone(&h.cache[i].infoschema));
        }
        None
    }

    /// GetBySnapshotTS gets the information schema based on snapshotTS.
    /// It searches the schema cache and find the schema with max schema ts that equals or
    /// smaller than given snapshot ts, where the schema ts is the commitTs of the txn
    /// creates the schema diff.
    // go: InfoCache.GetBySnapshotTS L1
    pub fn get_by_snapshot_ts(&self, snapshot_ts: u64) -> Option<Arc<InfoSchemaV2>> {
        let h = self.mu.read();
        Self::get_schema_by_timestamp_no_lock(&h, snapshot_ts)
    }

    /// Insert will **TRY** to insert the infoschema into the cache.
    /// It only promised to cache the newest infoschema.
    /// It returns 'true' if it is cached, 'false' otherwise.
    /// schemaTs is the commitTs of the txn creates the schema diff, which indicates since
    /// when the schema version is taking effect
    ///
    /// Go starts `gcOldVersion` on a new goroutine; the stub does no I/O and runs inline.
    // go: InfoCache.Insert L1
    pub fn insert(&self, is: Arc<InfoSchemaV2>, schema_ts: u64) -> bool {
        let version = is.schema_meta_version();
        log::debug!("INSERT SCHEMA, schema ts: {schema_ts}, schema version: {version}");
        let mut h = self.mu.write();

        if h.last_check_version == 0 {
            h.last_check_version = version;
            h.last_check_time = Some(Instant::now());
        } else if version > h.last_check_version + GC_CHECK_INTERVAL
            && h.last_check_time
                .is_none_or(|t| t.elapsed() > Duration::from_secs(60))
        {
            h.last_check_version = version;
            h.last_check_time = Some(Instant::now());
            self.gc_old_version(h.last_check_version);
        }

        // assume this is the timestamp order as well
        let i = search_version(&h.cache, version);

        // cached entry
        if i < h.cache.len() && h.cache[i].infoschema.schema_meta_version() == version {
            // IsV2 is true for both (spec §1.2).
            // update timestamp if it is not 0 and cached one is 0
            if schema_ts > 0 && h.cache[i].timestamp == 0 {
                h.cache[i].timestamp = schema_ts as i64;
            } else {
                // update infoschema if it's infoschema v2
                h.cache[i].infoschema = is;
            }
            return true;
        }

        let entry = SchemaAndTimestamp {
            infoschema: is,
            timestamp: schema_ts as i64,
        };
        if h.cache.len() < h.capacity {
            // has free space, grown the slice
            h.cache.insert(i, entry);
            if h.cache.len() == 1 {
                h.first_known_schema_version = version;
            }
        } else if i < h.cache.len() {
            // drop older schema
            h.cache.pop();
            h.cache.insert(i, entry);
        } else {
            // older than all cached schemas, refuse to cache it
            return false;
        }

        true
    }

    /// InsertEmptySchemaVersion inserts empty schema version into a map. If exceeded the
    /// cache capacity, remove the oldest version.
    // go: InfoCache.InsertEmptySchemaVersion L1
    pub fn insert_empty_schema_version(&self, version: i64) {
        let mut h = self.mu.write();
        h.empty_schema_versions.insert(version, ());
        if h.empty_schema_versions.len() > h.capacity {
            // remove oldest version.
            let mut versions: Vec<i64> = h.empty_schema_versions.keys().copied().collect();
            versions.sort_unstable();
            for ver in versions {
                h.empty_schema_versions.remove(&ver);
                if h.empty_schema_versions.len() <= h.capacity {
                    break;
                }
            }
        }
    }

    /// `gcOldVersion`: `meta.GetOldestSchemaVersion` reads the MVCC history of the schema
    /// version key with `store/helper` (TiKV's MVCC debug API, not ported); the old items
    /// stay in `Data` until then (spec §10).
    // glue: stub of InfoCache.gcOldVersion, blocked-by:D06.2 (meta.GetOldestSchemaVersion needs store/helper's MVCC API)
    fn gc_old_version(&self, current_version: i64) {
        let _ = &self.r;
        log::info!(
            "GC compact old schema version skipped: meta.GetOldestSchemaVersion is not ported, current version: {current_version}"
        );
    }
}

#[cfg(test)]
mod tests {
    //! `pkg/infoschema/test/cachetest/cache_test.go`. Go's
    //! `MockInfoSchemaWithSchemaVer` (v1, not ported) is an empty v2 InfoSchema with the
    //! version set ([`mock`]); Go's `require.Equal(isN, ...)` compares the version.

    use super::*;
    use crate::infoschema_v2::new_info_schema_v2;

    fn mock(ver: i64) -> Arc<InfoSchemaV2> {
        let mut is = new_info_schema_v2(None, None, new_data());
        Arc::make_mut(&mut is.misc).schema_meta_version = ver;
        Arc::new(is)
    }

    fn ver(is: Option<Arc<InfoSchemaV2>>) -> Option<i64> {
        is.map(|is| is.schema_meta_version())
    }

    #[test]
    fn new_cache_is_empty() {
        let ic = new_cache(None, 16);
        assert!(ic.get_latest().is_none());
    }

    #[test]
    fn insert() {
        let ic = new_cache(None, 3);
        ic.insert(mock(2), 2);
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), Some(2));
        assert_eq!(ver(ic.get_by_snapshot_ts(10)), Some(2));
        assert_eq!(ver(ic.get_by_snapshot_ts(0)), None);

        // newer
        ic.insert(mock(5), 5);
        assert_eq!(ver(ic.get_by_version(5)), Some(5));
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        // there is a gap in schema cache, so don't use this version
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(10)), Some(5));

        // older
        ic.insert(mock(0), 0);
        assert_eq!(ver(ic.get_by_version(5)), Some(5));
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        assert_eq!(ver(ic.get_by_version(0)), Some(0));

        // replace 5, drop 0
        ic.insert(mock(6), 6);
        assert_eq!(ver(ic.get_by_version(6)), Some(6));
        assert_eq!(ver(ic.get_by_version(5)), Some(5));
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        assert_eq!(ver(ic.get_by_version(0)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(5)), Some(5));
        assert_eq!(ver(ic.get_by_snapshot_ts(10)), Some(6));

        // replace 2, drop 2
        ic.insert(mock(3), 3);
        assert_eq!(ver(ic.get_by_version(6)), Some(6));
        assert_eq!(ver(ic.get_by_version(5)), Some(5));
        assert_eq!(ver(ic.get_by_version(3)), Some(3));
        assert_eq!(ver(ic.get_by_version(2)), None);
        assert_eq!(ver(ic.get_by_version(0)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(10)), Some(6));

        // insert 2, but failed silently
        assert!(!ic.insert(mock(2), 2));
        assert_eq!(ver(ic.get_by_version(3)), Some(3));
        assert_eq!(ver(ic.get_by_version(2)), None);

        // insert 5, but it is already in
        let is5 = ic.get_by_version(5).unwrap();
        ic.insert(Arc::clone(&is5), 5);
        assert!(Arc::ptr_eq(&ic.get_by_version(5).unwrap(), &is5));
        assert_eq!(ver(ic.get_by_snapshot_ts(5)), Some(5));
        assert_eq!(ver(ic.get_by_snapshot_ts(10)), Some(6));
    }

    #[test]
    fn get_by_version() {
        let ic = new_cache(None, 2);
        ic.insert(mock(1), 1);
        ic.insert(mock(3), 3);
        assert_eq!(ver(ic.get_by_version(1)), Some(1));
        assert_eq!(ver(ic.get_by_version(3)), Some(3));
        assert_eq!(ver(ic.get_by_version(0)), None, "index == 0, but not found");
        assert_eq!(ver(ic.get_by_version(2)), Some(1));
        assert_eq!(
            ver(ic.get_by_version(4)),
            None,
            "index == length, but not found"
        );
    }

    #[test]
    fn get_latest() {
        let ic = new_cache(None, 16);
        assert!(ic.get_latest().is_none());
        ic.insert(mock(1), 1);
        assert_eq!(ver(ic.get_latest()), Some(1));
        // newer change the newest
        ic.insert(mock(2), 2);
        assert_eq!(ver(ic.get_latest()), Some(2));
        // older schema doesn't change the newest
        ic.insert(mock(0), 0);
        assert_eq!(ver(ic.get_latest()), Some(2));
    }

    #[test]
    fn get_by_timestamp() {
        let ic = new_cache(None, 16);
        assert_eq!(ic.len(), 0);
        ic.insert(mock(1), 1);
        assert_eq!(ver(ic.get_by_snapshot_ts(0)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(1)), Some(1));
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), Some(1));
        assert_eq!(ic.len(), 1);

        ic.insert(mock(3), 3);
        assert_eq!(ver(ic.get_latest()), Some(3));
        assert_eq!(ver(ic.get_by_snapshot_ts(0)), None);
        // there is a gap, no schema returned for ts 2
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(3)), Some(3));
        assert_eq!(ver(ic.get_by_snapshot_ts(4)), Some(3));
        assert_eq!(ic.len(), 2);

        // schema version 2 doesn't have timestamp set
        ic.insert(mock(2), 0);
        assert_eq!(ver(ic.get_latest()), Some(3));
        assert_eq!(ver(ic.get_by_snapshot_ts(0)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(1)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), None);
        assert_eq!(ver(ic.get_by_snapshot_ts(3)), Some(3));
        assert_eq!(ver(ic.get_by_snapshot_ts(4)), Some(3));
        assert_eq!(ic.len(), 3);

        // insert is2 again with correct timestamp, to correct previous wrong timestamp
        ic.insert(mock(2), 2);
        assert_eq!(ver(ic.get_latest()), Some(3));
        assert_eq!(ver(ic.get_by_snapshot_ts(1)), Some(1));
        assert_eq!(ver(ic.get_by_snapshot_ts(2)), Some(2));
        assert_eq!(ver(ic.get_by_snapshot_ts(3)), Some(3));
        assert_eq!(ic.len(), 3);
    }

    #[test]
    fn re_size() {
        let ic = new_cache(None, 2);
        ic.insert(mock(1), 1);
        ic.insert(mock(2), 2);

        ic.re_size(3);
        assert_eq!(ic.size(), 2);
        assert_eq!(ver(ic.get_by_version(1)), Some(1));
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        assert!(ic.insert(mock(3), 3));
        assert_eq!(ver(ic.get_by_version(1)), Some(1));
        assert_eq!(ver(ic.get_by_version(2)), Some(2));
        assert_eq!(ver(ic.get_by_version(3)), Some(3));

        ic.re_size(1);
        assert_eq!(ic.size(), 1);
        assert_eq!(ver(ic.get_by_version(1)), None);
        assert_eq!(ver(ic.get_by_version(2)), None);
        assert_eq!(ver(ic.get_by_version(3)), Some(3));
        assert!(!ic.insert(mock(2), 2));
        assert_eq!(ic.size(), 1);
        assert!(ic.insert(mock(4), 4));
        assert_eq!(ic.size(), 1);
        assert_eq!(ver(ic.get_by_version(3)), None);
        assert_eq!(ver(ic.get_by_version(4)), Some(4));
    }

    #[test]
    fn cache_with_schema_ts_zero() {
        let check = |ic: &InfoCache, start: i64, end: i64, exist: bool| {
            assert!(start <= end);
            let latest = ic.get_latest().unwrap().schema_meta_version();
            for ts in start..=end {
                let is = ic.get_by_snapshot_ts(ts as u64);
                if exist {
                    let v = ver(is).unwrap_or_else(|| panic!("ts {ts}"));
                    assert_eq!(v, ts.min(latest), "ts {ts}");
                } else {
                    assert!(is.is_none(), "ts {ts}");
                }
            }
        };
        let ic = new_cache(None, 16);
        for i in 1..=8 {
            ic.insert(mock(i), i as u64);
        }
        check(&ic, 1, 8, true);
        check(&ic, 8, 10, true);

        // mock for meet error There is no Write MVCC info for the schema version
        ic.insert(mock(9), 0);
        check(&ic, 1, 7, true);
        check(&ic, 8, 9, false);
        check(&ic, 9, 10, false);

        for i in 10..=16 {
            ic.insert(mock(i), i as u64);
            check(&ic, 1, 7, true);
            check(&ic, 8, 9, false);
            check(&ic, 10, 16, true);
        }
        assert_eq!(ic.size(), 16);

        // refill the cache
        ic.insert(mock(9), 9);
        check(&ic, 1, 16, true);
        assert_eq!(ic.size(), 16);

        // Test more than capacity
        ic.insert(mock(17), 17);
        check(&ic, 1, 1, false);
        check(&ic, 2, 17, true);
        check(&ic, 2, 20, true);
        assert_eq!(ic.size(), 16);

        // Test for there is a hole in the middle.
        let ic = new_cache(None, 16);
        // mock for restart with full load the latest version schema.
        ic.insert(mock(100), 100);
        check(&ic, 1, 99, false);
        check(&ic, 100, 100, true);
        for i in 1..=16 {
            ic.insert(mock(i), i as u64);
        }
        check(&ic, 1, 1, false);
        check(&ic, 2, 15, true);
        check(&ic, 16, 16, false);
        check(&ic, 100, 100, true);
        assert_eq!(ic.size(), 16);
        for i in 85..100 {
            ic.insert(mock(i), i as u64);
        }
        check(&ic, 1, 84, false);
        check(&ic, 85, 100, true);
        assert_eq!(ic.size(), 16);

        // Test cache with schema version hole, which is cause by schema version doesn't has related schema-diff.
        let ic = new_cache(None, 16);
        for i in 1..=8 {
            ic.insert(mock(i), i as u64);
        }
        check(&ic, 1, 10, true);
        // mock for schema version hole, schema-version 9 is missing.
        ic.insert(mock(10), 10);
        check(&ic, 1, 7, true);
        // without empty schema version map, get snapshot by ts 8, 9 will both failed.
        check(&ic, 8, 9, false);
        check(&ic, 10, 10, true);
        // add empty schema version 9.
        ic.insert_empty_schema_version(9);
        // after set empty schema version, get snapshot by ts 8, 9 will both success.
        check(&ic, 1, 8, true);
        check(&ic, 10, 10, true);
        // since schema version 9 is empty, so get by ts 9 will get schema which version is 8.
        assert_eq!(ver(ic.get_by_snapshot_ts(9)), Some(8));
    }

    #[test]
    fn cache_empty_schema_version() {
        let ic = new_cache(None, 16);
        assert_eq!(ic.get_empty_schema_versions().len(), 0);
        for i in 0..16 {
            ic.insert_empty_schema_version(i);
        }
        let empty = ic.get_empty_schema_versions();
        assert_eq!(empty.len(), 16);
        assert!((0..16).all(|i| empty.contains_key(&i)));
        for i in 16..20 {
            ic.insert_empty_schema_version(i);
        }
        let empty = ic.get_empty_schema_versions();
        assert_eq!(empty.len(), 16);
        assert!((4..20).all(|i| empty.contains_key(&i)));
    }

    /// N13: a version that leaves the cache is freed once no statement holds it.
    #[test]
    fn evicted_versions_are_freed() {
        let ic = new_cache(None, 2);
        let first = mock(1);
        let weak = Arc::downgrade(&first);
        ic.insert(first, 1);
        ic.insert(mock(2), 2);
        let held = ic.get_by_version(2).unwrap();
        ic.insert(mock(3), 3);
        assert!(
            weak.upgrade().is_none(),
            "version 1 left the cache and is freed"
        );
        ic.insert(mock(4), 4);
        // Version 2 left the cache but a statement still holds it.
        assert_eq!(held.schema_meta_version(), 2);
        assert_eq!(ver(ic.get_by_version(2)), None);
    }
}
