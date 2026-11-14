//! go-file: pkg/infoschema/sieve.go
//!
//! The SIEVE cache of infoschema v2's table cache (`Data.tableCache`). Exception A3
//! (specs/D06.3-infoschema/spec.md §5): the eviction policy is Go's (SIEVE, the same hand
//! movement, the same "at most 10 evictions per `Set`"), the memory layout is Rust's:
//! - the list is a slab of nodes linked by index instead of `container/list` elements;
//! - an entry's size is [`SieveSize`] of its key and value plus the entry itself, instead
//!   of Go's reflection walk (`internal.Sizeof`, Go memory layout);
//! - Go's `ctx` / `cancel` (never read) are not kept.
//!
//! The hand of a purged cache is kept as the purged key ([`Hand::Stale`]): Go's
//! `Purge` leaves `hand` on a removed `list.Element` whose `Prev()` is nil and whose
//! `Value` is still the key, and the next `evict` starts from it.

use std::collections::HashMap;
use std::hash::Hash;

use parking_lot::Mutex;

/// The heap size of a cached key or value (exception A3: replaces Go's reflection-based
/// `internal.Sizeof`; the entry's own struct is added by [`Sieve`]).
// glue: A3 (Rust memory layout of the cache entries)
pub trait SieveSize {
    // glue: A3
    fn heap_size(&self) -> u64;
}

impl SieveSize for i64 {
    // glue: A3
    fn heap_size(&self) -> u64 {
        0
    }
}

impl SieveSize for String {
    // glue: A3
    fn heap_size(&self) -> u64 {
        self.capacity() as u64
    }
}

/// entry holds the key and value of a cache entry.
struct Entry<K, V> {
    key: K,
    value: V,
    visited: bool,
    /// The node of the entry in the list.
    element: usize,
    size: u64,
}

impl<K: SieveSize, V: SieveSize> Entry<K, V> {
    // go: entry.Size L2 (A3: the Rust entry's struct plus the heap of key and value, not Go's reflection walk)
    fn size(&mut self) -> u64 {
        if self.size == 0 {
            let size =
                std::mem::size_of::<Self>() as u64 + self.key.heap_size() + self.value.heap_size();
            if size > 0 {
                self.size = size;
            }
        }
        self.size
    }
}

/// A list node (Go `list.Element`): the key, linked by slab index.
struct Node<K> {
    key: K,
    /// Towards the front (Go `Element.Prev`).
    prev: Option<usize>,
    /// Towards the back (Go `Element.Next`).
    next: Option<usize>,
}

/// Where the next eviction starts.
enum Hand<K> {
    /// Go `hand == nil`: start from the back.
    None,
    /// A node in the list.
    Node(usize),
    /// A node removed by `Purge` (Go keeps the removed element): its key is looked up again,
    /// and its `Prev()` is nil.
    Stale(K),
}

/// The doubly linked list of keys (Go `container/list`), front = most recently inserted.
struct List<K> {
    nodes: Vec<Option<Node<K>>>,
    free: Vec<usize>,
    front: Option<usize>,
    back: Option<usize>,
    len: usize,
}

impl<K: Clone> List<K> {
    // glue: container/list.New
    fn new() -> Self {
        List {
            nodes: Vec::new(),
            free: Vec::new(),
            front: None,
            back: None,
            len: 0,
        }
    }

    // glue: container/list.PushFront
    fn push_front(&mut self, key: K) -> usize {
        let node = Node {
            key,
            prev: None,
            next: self.front,
        };
        let idx = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some(node);
                i
            }
            None => {
                self.nodes.push(Some(node));
                self.nodes.len() - 1
            }
        };
        if let Some(f) = self.front {
            self.node_mut(f).prev = Some(idx);
        } else {
            self.back = Some(idx);
        }
        self.front = Some(idx);
        self.len += 1;
        idx
    }

    // glue: container/list.Remove
    fn remove(&mut self, idx: usize) {
        let node = self.nodes[idx]
            .take()
            .expect("sieve: removing a free list node");
        match node.prev {
            Some(p) => self.node_mut(p).next = node.next,
            None => self.front = node.next,
        }
        match node.next {
            Some(n) => self.node_mut(n).prev = node.prev,
            None => self.back = node.prev,
        }
        self.free.push(idx);
        self.len -= 1;
    }

    // glue: container/list.Element.Prev
    fn prev(&self, idx: usize) -> Option<usize> {
        self.node(idx).prev
    }

    // glue: container/list.Element.Value
    fn key(&self, idx: usize) -> &K {
        &self.node(idx).key
    }

    // glue: container/list.Init
    fn init(&mut self) {
        self.nodes.clear();
        self.free.clear();
        self.front = None;
        self.back = None;
        self.len = 0;
    }

    // glue: slab access
    fn node(&self, idx: usize) -> &Node<K> {
        self.nodes[idx].as_ref().expect("sieve: dangling list node")
    }

    // glue: slab access
    fn node_mut(&mut self, idx: usize) -> &mut Node<K> {
        self.nodes[idx].as_mut().expect("sieve: dangling list node")
    }
}

/// The status callbacks of a [`Sieve`] (Go `sieveStatusHook`); the metrics implementation
/// (`sieveStatusHookImpl`, pkg/infoschema/metrics.go) is block D06.6.
pub trait SieveStatusHook: Send + Sync {
    // glue: Go interface method sieveStatusHook.onHit
    fn on_hit(&self);
    // glue: Go interface method sieveStatusHook.onMiss
    fn on_miss(&self);
    // glue: Go interface method sieveStatusHook.onEvict
    fn on_evict(&self);
    // glue: Go interface method sieveStatusHook.onUpdateSize
    fn on_update_size(&self, size: u64);
    // glue: Go interface method sieveStatusHook.onUpdateLimit
    fn on_update_limit(&self, limit: u64);
}

/// The hook that does nothing.
pub struct EmptySieveStatusHook;

impl SieveStatusHook for EmptySieveStatusHook {
    // go: emptySieveStatusHook.onHit L1
    fn on_hit(&self) {}

    // go: emptySieveStatusHook.onMiss L1
    fn on_miss(&self) {}

    // go: emptySieveStatusHook.onEvict L1
    fn on_evict(&self) {}

    // go: emptySieveStatusHook.onUpdateSize L1
    fn on_update_size(&self, _size: u64) {}

    // go: emptySieveStatusHook.onUpdateLimit L1
    fn on_update_limit(&self, _limit: u64) {}
}

struct Inner<K, V> {
    size: u64,
    /// capacity can be set to zero for disabling infoschema v2
    capacity: u64,
    items: HashMap<K, Entry<K, V>>,
    ll: List<K>,
    hand: Hand<K>,
    hook: Box<dyn SieveStatusHook>,
}

/// Sieve is an efficient turn-Key eviction algorithm for web caches.
/// See blog post https://cachemon.github.io/SIEVE-website/blog/2023/12/17/sieve-is-simpler-than-lru/
/// and also the academic paper "SIEVE is simpler than LRU"
pub struct Sieve<K, V> {
    mu: Mutex<Inner<K, V>>,
}

/// newSieve creates a cache of `capacity` bytes.
// go: newSieve L1 [M-LOCK]
pub fn new_sieve<K, V>(capacity: u64) -> Sieve<K, V>
where
    K: Eq + Hash + Clone + SieveSize,
    V: Clone + SieveSize,
{
    Sieve {
        mu: Mutex::new(Inner {
            size: 0,
            capacity,
            items: HashMap::new(),
            ll: List::new(),
            hand: Hand::None,
            hook: Box::new(EmptySieveStatusHook),
        }),
    }
}

impl<K, V> Sieve<K, V>
where
    K: Eq + Hash + Clone + SieveSize,
    V: Clone + SieveSize,
{
    // go: Sieve.SetStatusHook L1 [M-LOCK]
    pub fn set_status_hook(&self, hook: Box<dyn SieveStatusHook>) {
        self.mu.lock().hook = hook;
    }

    // go: Sieve.SetCapacity L1 [M-LOCK]
    pub fn set_capacity(&self, capacity: u64) {
        let mut s = self.mu.lock();
        s.capacity = capacity;
        s.hook.on_update_limit(capacity);
    }

    // go: Sieve.SetCapacityAndWaitEvict L1 [M-LOCK]
    pub fn set_capacity_and_wait_evict(&self, capacity: u64) {
        self.set_capacity(capacity);
        loop {
            let mut s = self.mu.lock();
            if s.size <= s.capacity {
                break;
            }
            let mut i = 0;
            while s.size > s.capacity && i < 10 {
                s.evict();
                i += 1;
            }
        }
    }

    // go: Sieve.Capacity L1 [M-LOCK]
    pub fn capacity(&self) -> u64 {
        self.mu.lock().capacity
    }

    // go: Sieve.Set L1 [M-LOCK]
    pub fn set(&self, key: K, value: V) {
        let mut s = self.mu.lock();

        if let Some(e) = s.items.get_mut(&key) {
            e.value = value;
            e.visited = true;
            return;
        }

        let mut i = 0;
        while s.size > s.capacity && i < 10 {
            s.evict();
            i += 1;
        }

        let mut e = Entry {
            key: key.clone(),
            value,
            visited: false,
            element: 0,
            size: 0,
        };
        s.size += e.size(); // calculate the size first without putting to the list.
        let size = s.size;
        s.hook.on_update_size(size);
        e.element = s.ll.push_front(key.clone());

        s.items.insert(key, e);
    }

    // go: Sieve.Get L1 [M-LOCK]
    pub fn get(&self, key: &K) -> Option<V> {
        let mut s = self.mu.lock();
        let s = &mut *s;
        if let Some(e) = s.items.get_mut(key) {
            e.visited = true;
            s.hook.on_hit();
            return Some(e.value.clone());
        }
        s.hook.on_miss();
        None
    }

    // go: Sieve.Remove L1 [M-LOCK]
    pub fn remove(&self, key: &K) -> bool {
        let mut s = self.mu.lock();

        if let Some(element) = s.items.get(key).map(|e| e.element) {
            // if the element to be removed is the hand,
            // then move the hand to the previous one.
            if matches!(s.hand, Hand::Node(h) if h == element) {
                s.hand = match s.ll.prev(element) {
                    Some(p) => Hand::Node(p),
                    None => Hand::None,
                };
            }

            s.remove_entry(key);
            return true;
        }

        false
    }

    // go: Sieve.Contains L1 [M-LOCK]
    pub fn contains(&self, key: &K) -> bool {
        self.mu.lock().items.contains_key(key)
    }

    // go: Sieve.Peek L1 [M-LOCK]
    pub fn peek(&self, key: &K) -> Option<V> {
        self.mu.lock().items.get(key).map(|e| e.value.clone())
    }

    // go: Sieve.Size L1 [M-LOCK]
    pub fn size(&self) -> u64 {
        self.mu.lock().size
    }

    // go: Sieve.Len L1 [M-LOCK]
    pub fn len(&self) -> usize {
        self.mu.lock().ll.len
    }

    /// Whether the cache has no entry (`Len() == 0`).
    // glue: clippy len_without_is_empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // go: Sieve.Purge L1 [M-LOCK]
    pub fn purge(&self) {
        let mut s = self.mu.lock();

        // Go's hand keeps pointing at its (now removed) element.
        if let Hand::Node(h) = s.hand {
            let key = s.ll.key(h).clone();
            s.hand = Hand::Stale(key);
        }
        let keys: Vec<K> = s.items.keys().cloned().collect();
        for k in &keys {
            s.remove_entry(k);
        }

        s.ll.init();
    }

    /// Close purges the cache (Go also cancels the unused context).
    // go: Sieve.Close L1
    pub fn close(&self) {
        self.purge();
    }
}

impl<K, V> Inner<K, V>
where
    K: Eq + Hash + Clone + SieveSize,
    V: Clone + SieveSize,
{
    // go: Sieve.removeEntry L1
    fn remove_entry(&mut self, key: &K) {
        let Some(mut e) = self.items.remove(key) else {
            return;
        };
        if self.ll.nodes.get(e.element).is_some_and(Option::is_some) {
            self.ll.remove(e.element);
        }
        self.size -= e.size();
        self.hook.on_update_size(self.size);
    }

    // go: Sieve.evict L1
    fn evict(&mut self) {
        // if o is nil, then assign it to the tail element in the list
        let (mut o, mut key) = match &self.hand {
            Hand::Node(h) => (Some(*h), self.ll.key(*h).clone()),
            Hand::Stale(k) => (None, k.clone()),
            Hand::None => {
                let b = self.ll.back.expect("sieve: evicting from an empty list");
                (Some(b), self.ll.key(b).clone())
            }
        };

        if !self.items.contains_key(&key) {
            panic!("sieve: evicting non-existent element");
        }

        while self.items[&key].visited {
            self.items.get_mut(&key).expect("checked").visited = false;
            // A stale (purged) element's Prev() is nil.
            o = match o.and_then(|x| self.ll.prev(x)) {
                Some(p) => Some(p),
                None => self.ll.back,
            };
            let x = o.expect("sieve: evicting from an empty list");
            key = self.ll.key(x).clone();

            if !self.items.contains_key(&key) {
                panic!("sieve: evicting non-existent element");
            }
        }

        self.hand = match o.and_then(|x| self.ll.prev(x)) {
            Some(p) => Hand::Node(p),
            None => Hand::None,
        };
        self.remove_entry(&key);
        self.hook.on_evict();
    }
}

#[cfg(test)]
mod tests {
    //! go-file: pkg/infoschema/sieve_test.go
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn entry_size() -> u64 {
        std::mem::size_of::<Entry<i64, i64>>() as u64
    }

    #[test]
    fn test_get_and_set() {
        let items: Vec<i64> = (1..=10).collect();
        let cache = new_sieve::<i64, i64>(10 * MB);
        for &v in &items {
            cache.set(v, v * 10);
        }
        for &v in &items {
            assert_eq!(cache.get(&v), Some(v * 10));
        }
        cache.close();
    }

    #[test]
    fn test_remove() {
        let cache = new_sieve::<i64, i64>(10 * MB);
        cache.set(1, 10);
        assert_eq!(cache.get(&1), Some(10));
        // After removing the key, it should not be found
        assert!(cache.remove(&1));
        assert_eq!(cache.get(&1), None);
        // This should not panic
        assert!(!cache.remove(&-1));
        cache.close();
    }

    #[test]
    fn test_sieve_policy() {
        let cache = new_sieve::<i64, i64>(10 * entry_size());
        let one_hit_wonders = [1, 2, 3, 4, 5];
        let popular_objects = [6, 7, 8, 9, 10];
        // add objects to the cache
        for &v in one_hit_wonders.iter().chain(&popular_objects) {
            cache.set(v, v);
        }
        // hit popular objects
        for v in &popular_objects {
            assert!(cache.get(v).is_some());
        }
        // add another objects to the cache
        for &v in &one_hit_wonders {
            cache.set(v * 10, v * 10);
        }
        // check popular objects are not evicted
        for v in &popular_objects {
            assert!(cache.get(v).is_some());
        }
        cache.close();
    }

    #[test]
    fn test_contains() {
        let cache = new_sieve::<String, String>(10 * MB);
        assert!(!cache.contains(&"hello".to_owned()));
        cache.set("hello".to_owned(), "world".to_owned());
        assert!(cache.contains(&"hello".to_owned()));
        cache.close();
    }

    #[test]
    fn test_cache_size() {
        let sz = entry_size();
        let cache = new_sieve::<i64, i64>(10 * MB);
        assert_eq!(cache.size(), 0);
        cache.set(1, 1);
        assert_eq!(cache.size(), sz);
        // duplicated keys only update the recent-ness of the key and value
        cache.set(1, 1);
        assert_eq!(cache.size(), sz);
        cache.set(2, 2);
        assert_eq!(cache.size(), 2 * sz);
        cache.close();
    }

    #[test]
    fn test_purge() {
        let cache = new_sieve::<i64, i64>(10 * MB);
        cache.set(1, 1);
        cache.set(2, 2);
        assert_eq!(cache.len(), 2);
        cache.purge();
        assert_eq!(cache.len(), 0);
        cache.close();
    }
}
