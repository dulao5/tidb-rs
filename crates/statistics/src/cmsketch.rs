//! go-file: pkg/statistics/cmsketch.go
//!
//! D11.1 part 1 (VS1-29) ports the read part of `TopN` that the column / index statistics
//! need (`Num`, `TotalCount`, `Copy`, `MemoryUsage`). Everything else of this file — the
//! Count-Min sketch, building and merging TopN, `QueryTopN` / `BetweenCount` (used only by
//! the non-pseudo estimation paths, which are boundary stubs) and the protobuf / storage
//! encodings — is D11.1 part 2 (specs/D11.1-statistics §1.3).
//!
//! `CMSketch` is a type without values until part 2 ports it: no statistics can hold one
//! (they are never built or loaded in part 1), so every method on it is statically
//! unreachable, and `Option<Arc<CMSketch>>` is always `None`.
//!
//! `TopN` (spec §2, N13 rule 3): Go caches the total count in the shared object on first
//! read (`sync.Once` in `calculateCount`), a write through a shared pointer. The Rust `TopN`
//! computes the total when it is constructed and its items are private, so the struct has no
//! interior mutability and the total cannot go stale.

/// CMSketch is used to estimate point queries.
// glue: boundary of the type CMSketch, blocked-by:D11.1 (part 2: Count-Min sketch; no statistics hold one in part 1)
#[derive(Debug)]
pub enum CMSketch {}

impl CMSketch {
    /// Copy makes a copy for current CMSketch.
    // glue: stub of CMSketch.Copy, blocked-by:D11.1 (part 2; no value of CMSketch exists)
    pub fn copy(&self) -> CMSketch {
        match *self {}
    }

    /// MemoryUsage returns the total memory usage of a CMSketch.
    // glue: stub of CMSketch.MemoryUsage, blocked-by:D11.1 (part 2; no value of CMSketch exists)
    pub fn memory_usage(&self) -> i64 {
        match *self {}
    }
}

/// TopNMeta stores the unit of the TopN.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopNMeta {
    /// Encoded.
    pub encoded: Vec<u8>,
    /// Count.
    pub count: u64,
}

/// TopN stores most-common values, which is used to estimate point queries.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TopN {
    top_n: Vec<TopNMeta>,
    total_count: u64,
}

impl TopN {
    /// A TopN with the given items; the total count is computed here (Go computes it on the
    /// first `TotalCount` through `sync.Once`, module doc).
    // glue: constructor (Go `&TopN{TopN: items}`; N13 rule 3)
    pub fn new(top_n: Vec<TopNMeta>) -> TopN {
        let total_count = calculate_count_internal(&top_n);
        TopN { top_n, total_count }
    }

    /// The items (Go's exported field `TopN`, read-only here).
    // glue: accessor (Go field TopN.TopN)
    pub fn top_n(&self) -> &[TopNMeta] {
        &self.top_n
    }

    /// Copy makes a copy for current TopN.
    // go: TopN.Copy L1
    pub fn copy(&self) -> TopN {
        let mut top_n = Vec::with_capacity(self.top_n.len());
        for t in &self.top_n {
            top_n.push(TopNMeta {
                encoded: t.encoded.to_vec(),
                count: t.count,
            });
        }
        TopN::new(top_n)
    }

    /// MemoryUsage returns the total memory usage of a topn.
    // go: TopN.MemoryUsage L1
    pub fn memory_usage(&self) -> i64 {
        let mut sum = 32; // size of array (24) + reference (8)
        for meta in &self.top_n {
            sum += 32 + meta.encoded.capacity() as i64; // 32 is size of byte array (24) + size of uint64 (8)
        }
        sum
    }
}

/// Num returns the ndv of the TopN.
///
///  TopN is declared directly in Histogram. So the Len is occupied by the Histogram. We use Num instead.
///
/// Go's nil receiver is `None`.
// go: TopN.Num L1
pub fn top_n_num(c: Option<&TopN>) -> usize {
    match c {
        None => 0,
        Some(c) => c.top_n.len(),
    }
}

/// TotalCount returns how many data is stored in TopN.
///
/// The count is computed at construction (module doc).
// go: TopN.TotalCount L1
pub fn top_n_total_count(c: Option<&TopN>) -> u64 {
    match c {
        None => 0,
        Some(c) if c.top_n.is_empty() => 0,
        Some(c) => c.total_count,
    }
}

/// calculateCountInternal.
// go: TopN.calculateCountInternal L1
fn calculate_count_internal(top_n: &[TopNMeta]) -> u64 {
    let mut total: u64 = 0;
    for t in top_n {
        total = total.wrapping_add(t.count);
    }
    total
}

// glue: stub of TopN.calculateCount, blocked-by:D11.1 (replaced by TopN::new; spec §2)
// glue: stub of TopN.onceCalculateCount, blocked-by:D11.1 (replaced by TopN::new; spec §2)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_n_total_is_computed_at_construction() {
        let t = TopN::new(vec![
            TopNMeta {
                encoded: vec![1],
                count: 3,
            },
            TopNMeta {
                encoded: vec![2, 3],
                count: 4,
            },
        ]);
        assert_eq!(top_n_total_count(Some(&t)), 7);
        assert_eq!(top_n_num(Some(&t)), 2);
        assert_eq!(top_n_total_count(None), 0);
        let c = t.copy();
        assert_eq!(c, t);
        assert_eq!(c.memory_usage(), 32 + 33 + 34);
    }
}
