//! go-file: pkg/util/kvcache/simple_lru.go
//!
//! `pkg/util/kvcache` `SimpleLRUCache` (block D00.3, ported whole per constitution 4.4 into
//! the lowest crate that uses it: `SessionVars`' non-prepared plan-cache statement cache and
//! prepare dedup cache). Not thread-safe, like Go's. `GlobalLRUMemUsageTracker` is not
//! ported: nothing in v8.5.8 consumes it (`init` creates it, no one attaches to it).
//!
//! Go stores `any` values and `Key` interfaces; here the cache is generic over the key and
//! value types (keys are compared by their `hash()` bytes, as in Go).

use std::collections::HashMap;

use tidb_base::Error;

/// Key is the interface that every key in LRU Cache should implement.
pub trait Key {
    /// The bytes the cache indexes the key by.
    // glue: kvcache.Key.Hash (interface method)
    fn hash(&self) -> Vec<u8>;
}

const NIL: usize = usize::MAX;

/// cacheEntry wraps Key and Value. It's the value of list.Element.
struct CacheEntry<K, V> {
    key: K,
    value: V,
    prev: usize,
    next: usize,
}

/// The evict callback (Go `func(Key, Value)`).
pub type OnEvict<K, V> = Box<dyn FnMut(&K, &V) + Send>;

/// SimpleLRUCache is a simple least recently used cache, not thread-safe, use it carefully.
pub struct SimpleLRUCache<K, V> {
    elements: HashMap<Vec<u8>, usize>,
    /// onEvict function will be called if any eviction happened
    on_evict: Option<OnEvict<K, V>>,
    /// The `container/list` of Go: a doubly linked list in a slab, front = most recent.
    slots: Vec<Option<CacheEntry<K, V>>>,
    free: Vec<usize>,
    front: usize,
    back: usize,
    capacity: usize,
    size: usize,
    /// 0 indicates no quota
    quota: u64,
    guard: f64,
}

impl<K: std::fmt::Debug, V> std::fmt::Debug for SimpleLRUCache<K, V> {
    // glue: Debug
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleLRUCache")
            .field("capacity", &self.capacity)
            .field("size", &self.size)
            .finish()
    }
}

/// NewSimpleLRUCache creates a SimpleLRUCache object, whose capacity is "capacity".
/// NOTE: "capacity" should be a positive value.
// go: NewSimpleLRUCache L1
pub fn new_simple_lru_cache<K: Key, V>(
    capacity: usize,
    guard: f64,
    quota: u64,
) -> SimpleLRUCache<K, V> {
    if capacity < 1 {
        panic!("capacity of LRU Cache should be at least 1.");
    }
    SimpleLRUCache {
        elements: HashMap::new(),
        on_evict: None,
        slots: Vec::new(),
        free: Vec::new(),
        front: NIL,
        back: NIL,
        capacity,
        size: 0,
        quota,
        guard,
    }
}

impl<K: Key, V> SimpleLRUCache<K, V> {
    // glue: container/list element access
    fn entry(&self, i: usize) -> &CacheEntry<K, V> {
        self.slots[i].as_ref().expect("live list element")
    }

    // glue: container/list element access
    fn entry_mut(&mut self, i: usize) -> &mut CacheEntry<K, V> {
        self.slots[i].as_mut().expect("live list element")
    }

    // glue: container/list Remove (unlink)
    fn unlink(&mut self, i: usize) {
        let (prev, next) = {
            let e = self.entry(i);
            (e.prev, e.next)
        };
        if prev == NIL {
            self.front = next;
        } else {
            self.entry_mut(prev).next = next;
        }
        if next == NIL {
            self.back = prev;
        } else {
            self.entry_mut(next).prev = prev;
        }
    }

    // glue: container/list PushFront (link)
    fn link_front(&mut self, i: usize) {
        let old = self.front;
        {
            let e = self.entry_mut(i);
            e.prev = NIL;
            e.next = old;
        }
        if old == NIL {
            self.back = i;
        } else {
            self.entry_mut(old).prev = i;
        }
        self.front = i;
    }

    // glue: container/list MoveToFront
    fn move_to_front(&mut self, i: usize) {
        if self.front != i {
            self.unlink(i);
            self.link_front(i);
        }
    }

    // glue: container/list Remove
    fn remove_elem(&mut self, i: usize) -> CacheEntry<K, V> {
        self.unlink(i);
        self.free.push(i);
        self.slots[i].take().expect("live list element")
    }

    /// SetOnEvict set the function called on each eviction.
    // go: SimpleLRUCache.SetOnEvict L1
    pub fn set_on_evict(&mut self, on_evict: OnEvict<K, V>) {
        self.on_evict = Some(on_evict);
    }

    /// Get tries to find the corresponding value according to the given key.
    // go: SimpleLRUCache.Get L1
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let i = *self.elements.get(&key.hash())?;
        self.move_to_front(i);
        Some(&self.entry(i).value)
    }

    /// Put puts the (key, value) pair into the LRU Cache.
    // go: SimpleLRUCache.Put L1
    pub fn put(&mut self, key: K, value: V) {
        let hash = key.hash();
        if let Some(&i) = self.elements.get(&hash) {
            self.entry_mut(i).value = value;
            self.move_to_front(i);
            return;
        }

        let entry = CacheEntry {
            key,
            value,
            prev: NIL,
            next: NIL,
        };
        let i = match self.free.pop() {
            Some(i) => {
                self.slots[i] = Some(entry);
                i
            }
            None => {
                self.slots.push(Some(entry));
                self.slots.len() - 1
            }
        };
        self.link_front(i);
        self.elements.insert(hash, i);
        self.size += 1;
        // Getting used memory is expensive and can be avoided by setting quota to 0.
        if self.quota == 0 {
            if self.size > self.capacity {
                let lru = self.back;
                let e = self.remove_elem(lru);
                if let Some(on_evict) = self.on_evict.as_mut() {
                    on_evict(&e.key, &e.value);
                }
                self.elements.remove(&e.key.hash());
                self.size -= 1;
            }
            return;
        }

        let Ok(mut mem_used) = tidb_memory::meminfo::instance_mem_used() else {
            self.delete_all();
            return;
        };

        let limit = (self.quota as f64 * (1.0 - self.guard)) as u64;
        while mem_used > limit || self.size > self.capacity {
            let lru = self.back;
            if lru == NIL {
                break;
            }
            // Go calls onEvict before removing the element.
            if let Some(on_evict) = self.on_evict.as_mut() {
                let e = self.slots[lru].as_ref().expect("live list element");
                on_evict(&e.key, &e.value);
            }
            let e = self.remove_elem(lru);
            self.elements.remove(&e.key.hash());
            self.size -= 1;
            if mem_used > limit {
                match tidb_memory::meminfo::instance_mem_used() {
                    Ok(m) => mem_used = m,
                    Err(_) => {
                        self.delete_all();
                        return;
                    }
                }
            }
        }
    }

    /// Delete deletes the key-value pair from the LRU Cache.
    // go: SimpleLRUCache.Delete L1
    pub fn delete(&mut self, key: &K) {
        let k = key.hash();
        let Some(i) = self.elements.remove(&k) else {
            return;
        };
        self.remove_elem(i);
        self.size -= 1;
    }

    /// DeleteAll deletes all elements from the LRU Cache.
    // go: SimpleLRUCache.DeleteAll L1
    pub fn delete_all(&mut self) {
        while self.back != NIL {
            let e = self.remove_elem(self.back);
            self.elements.remove(&e.key.hash());
            self.size -= 1;
        }
    }

    /// Size gets the current cache size.
    // go: SimpleLRUCache.Size L1
    pub fn size(&self) -> usize {
        self.size
    }

    /// Values return all values in cache (most recent first).
    // go: SimpleLRUCache.Values L1
    pub fn values(&self) -> Vec<&V> {
        let mut values = Vec::with_capacity(self.size);
        let mut i = self.front;
        while i != NIL {
            let e = self.entry(i);
            values.push(&e.value);
            i = e.next;
        }
        values
    }

    /// Keys return all keys in cache (most recent first).
    // go: SimpleLRUCache.Keys L1
    pub fn keys(&self) -> Vec<&K> {
        let mut keys = Vec::with_capacity(self.size);
        let mut i = self.front;
        while i != NIL {
            let e = self.entry(i);
            keys.push(&e.key);
            i = e.next;
        }
        keys
    }

    /// SetCapacity sets capacity of the cache.
    // Note: evicts without calling onEvict, as Go does.
    // go: SimpleLRUCache.SetCapacity L1
    pub fn set_capacity(&mut self, capacity: usize) -> Result<(), Error> {
        if capacity < 1 {
            return Err(Error::plain("capacity of lru cache should be at least 1"));
        }
        self.capacity = capacity;
        while self.size > self.capacity {
            let e = self.remove_elem(self.back);
            self.elements.remove(&e.key.hash());
            self.size -= 1;
        }
        Ok(())
    }

    /// RemoveOldest removes the oldest element from the cache.
    // go: SimpleLRUCache.RemoveOldest L1
    pub fn remove_oldest(&mut self) -> Option<(K, V)> {
        if self.size > 0 {
            let e = self.remove_elem(self.back);
            self.elements.remove(&e.key.hash());
            self.size -= 1;
            return Some((e.key, e.value));
        }
        None
    }
}
