//! A Go `map` literal as an immutable, sorted static table.
//!
//! Go package-level maps such as `mysql.MySQLErrName` are built once from a literal and
//! only read afterwards. The Rust port keeps them as `static` slices sorted by key (the
//! generated tables are emitted sorted), looked up by binary search. Iteration order is the
//! key order, which is deterministic; Go's map iteration is random, so no ported logic can
//! depend on it (mapping rule M-MAPORDER).

use std::borrow::Borrow;

/// glue: Go `map[K]V` built from a literal and never written.
pub struct GoMap<K: 'static, V: 'static> {
    entries: &'static [(K, V)],
}

impl<K: Ord, V> GoMap<K, V> {
    /// The entries must be sorted by key without duplicates (checked by `is_sorted` in tests).
    // glue: constructor of a generated table
    pub const fn new(entries: &'static [(K, V)]) -> Self {
        GoMap { entries }
    }

    /// `v, ok := m[k]`.
    // glue: Go map index
    pub fn get<Q: Ord + ?Sized>(&self, k: &Q) -> Option<&'static V>
    where
        K: Borrow<Q>,
    {
        self.entries
            .binary_search_by(|(ek, _)| ek.borrow().cmp(k))
            .ok()
            .map(|i| &self.entries[i].1)
    }

    /// `_, ok := m[k]`.
    // glue: Go map index
    pub fn contains_key<Q: Ord + ?Sized>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.get(k).is_some()
    }

    /// `len(m)`.
    // glue: Go len
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `len(m) == 0`.
    // glue: Go len
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `for k, v := range m`, in key order.
    // glue: Go range over map
    pub fn iter(&self) -> impl Iterator<Item = (&'static K, &'static V)> {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    /// Strictly increasing keys (the precondition of `get`).
    // glue: invariant check for tests
    pub fn is_sorted(&self) -> bool {
        self.entries.windows(2).all(|w| w[0].0 < w[1].0)
    }
}
