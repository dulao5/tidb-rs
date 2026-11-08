//! go-file: pkg/kv/cachedb.go
//!
//! The per-table value cache of cached tables (`MemManager`).
//!
//! Go keeps one `freecache.Cache` of 100 MiB per table. freecache is a third-party ring
//! buffer cache; [`FreeCache`] reproduces what `cacheDB` observes of it: `Set` rejects keys
//! longer than 65535 bytes and entries larger than a quarter of a segment (256 segments),
//! `Get` misses on unknown keys, and the cache holds at most its size. Eviction order is
//! insertion order (freecache evicts approximately the oldest entries of a segment's ring
//! buffer); a cached-table read that misses goes to the snapshot, so eviction order only
//! changes how often the snapshot is read (spec §4).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::RwLock;
use tidb_base::Error;

use crate::context::Context;
use crate::key::Key;
use crate::kv::Snapshot;
use crate::kv::get_value;

/// MemManager adds a cache between transaction buffer and the storage to reduce requests to
/// the storage. Beware, it uses table ID for partition tables, because the keys are unique
/// for partition tables. no matter the physical IDs are the same or not.
pub trait MemManager: Send + Sync {
    // glue: Go interface method MemManager.UnionGet
    /// UnionGet gets the value from cacheDB first, if it not exists,
    /// it gets the value from the snapshot, then caches the value in cacheDB.
    fn union_get<'a>(
        &'a self,
        ctx: &'a Context,
        tid: i64,
        snapshot: &'a dyn Snapshot,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>>;
    // glue: Go interface method MemManager.Delete
    /// Delete releases the cache by tableID.
    fn delete(&self, table_id: i64);
}

/// The behaviour of `github.com/coocood/freecache.Cache` that `cacheDB` relies on.
#[derive(Debug)]
pub struct FreeCache {
    max_key_val_len: usize,
    capacity: usize,
    used: usize,
    entries: HashMap<Vec<u8>, Vec<u8>>,
    order: VecDeque<Vec<u8>>,
}

/// freecache `ENTRY_HDR_SIZE`.
const ENTRY_HDR_SIZE: usize = 24;
/// freecache `segmentCount`.
const SEGMENT_COUNT: usize = 256;
/// freecache `minBufSize`.
const MIN_BUF_SIZE: usize = 512 * 1024;

impl FreeCache {
    /// `freecache.NewCache(size)`.
    // go-dep: freecache.NewCache L2 (no ring buffer; module docs)
    pub fn new(mut size: usize) -> FreeCache {
        if size < MIN_BUF_SIZE {
            size = MIN_BUF_SIZE;
        }
        let seg_size = size / SEGMENT_COUNT;
        FreeCache {
            max_key_val_len: seg_size / 4 - ENTRY_HDR_SIZE,
            capacity: size,
            used: 0,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// `Cache.Set(key, value, 0)`.
    // go-dep: freecache.Cache.Set L2 (insertion-order eviction; module docs)
    pub fn set(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if key.len() > 65535 {
            return Err(tidb_base::errors::new("The key is larger than 65535"));
        }
        if key.len() + value.len() > self.max_key_val_len {
            return Err(tidb_base::errors::new(
                "The entry size is larger than 1/1024 of cache size",
            ));
        }
        let size = ENTRY_HDR_SIZE + key.len() + value.len();
        if let Some(old) = self.entries.remove(key) {
            self.used -= ENTRY_HDR_SIZE + key.len() + old.len();
            self.order.retain(|k| k != key);
        }
        while self.used + size > self.capacity {
            let Some(k) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.entries.remove(&k) {
                self.used -= ENTRY_HDR_SIZE + k.len() + v.len();
            }
        }
        self.used += size;
        self.entries.insert(key.to_vec(), value.to_vec());
        self.order.push_back(key.to_vec());
        Ok(())
    }

    /// `Cache.Get(key)`: the value, or `ErrNotFound`.
    // go-dep: freecache.Cache.Get L1
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.entries.get(key).cloned()
    }

    /// `Cache.Clear()`.
    // go-dep: freecache.Cache.Clear L1
    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.used = 0;
    }
}

#[derive(Debug, Default)]
struct CacheDb {
    mem_tables: RwLock<HashMap<i64, FreeCache>>,
}

impl CacheDb {
    /// Set set the key/value in cacheDB.
    // go: cacheDB.set L1
    fn set(&self, table_id: i64, key: &Key, value: &[u8]) -> Result<(), Error> {
        let mut tables = self.mem_tables.write();
        let table = tables
            .entry(table_id)
            .or_insert_with(|| FreeCache::new(100 * 1024 * 1024));
        table.set(key, value)
    }

    /// Get gets the value from cacheDB.
    // go: cacheDB.get L1
    fn get(&self, table_id: i64, key: &Key) -> Option<Vec<u8>> {
        let tables = self.mem_tables.read();
        if let Some(table) = tables.get(&table_id)
            && let Some(val) = table.get(key)
        {
            return Some(val);
        }
        None
    }
}

impl MemManager for CacheDb {
    /// UnionGet implements MemManager UnionGet interface.
    ///
    /// Go tests `val == nil`: a value cached as empty bytes is a hit (freecache returns a
    /// non-nil empty slice), a miss is `None`.
    // go: cacheDB.UnionGet L1
    fn union_get<'a>(
        &'a self,
        ctx: &'a Context,
        tid: i64,
        snapshot: &'a dyn Snapshot,
        key: &'a Key,
    ) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            let val = self.get(tid, key);
            // key does not exist then get from snapshot and set to cache
            match val {
                Some(val) => Ok(val),
                None => {
                    let val = get_value(ctx, snapshot, key).await?;
                    self.set(tid, key, &val)?;
                    Ok(val)
                }
            }
        })
    }

    /// Delete delete and reset table from tables in cacheDB by tableID
    // go: cacheDB.Delete L1
    fn delete(&self, table_id: i64) {
        let mut tables = self.mem_tables.write();
        if let Some(k) = tables.get_mut(&table_id) {
            k.clear();
            tables.remove(&table_id);
        }
    }
}

/// NewCacheDB news the cacheDB.
// go: NewCacheDB L1
pub fn new_cache_db() -> Arc<dyn MemManager> {
    Arc::new(CacheDb::default())
}
