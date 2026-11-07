//! go-file: pkg/util/memory/memstats.go
//!
//! Go reads `runtime.ReadMemStats`; Rust has no Go runtime. The heap statistics come from
//! jemalloc, the global allocator of tidb-server (A1 3.2: `servermemorylimit` reads jemalloc
//! statistics, the same source TiKV uses; L3). Only the fields TiDB reads are kept:
//!
//! | Go `runtime.MemStats` | jemalloc | Meaning |
//! |---|---|---|
//! | `HeapAlloc` | `stats.allocated` | bytes in live allocations |
//! | `HeapInuse` | `stats.active` | bytes in active pages (≥ allocated) |
//! | `HeapSys` | `stats.mapped` | bytes mapped from the OS for the heap |
//! | `Sys` | `stats.resident` | resident bytes of the allocator |
//!
//! The `ReadMemStats` failpoint is not ported.

// Package-level names keep their Go names (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use arc_swap::ArcSwapOption;

/// The interval at which the global heap statistics are refreshed (Go `ReadMemInterval`).
pub const ReadMemInterval: Duration = Duration::from_millis(300);

/// The subset of Go `runtime.MemStats` TiDB reads (see the module docs for the sources).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStats {
    /// Go `HeapAlloc`.
    pub heap_alloc: u64,
    /// Go `HeapInuse`.
    pub heap_inuse: u64,
    /// Go `HeapSys`.
    pub heap_sys: u64,
    /// Go `Sys`.
    pub sys: u64,
}

/// Go `globalMstats`.
#[derive(Debug)]
struct GlobalMstats {
    #[allow(dead_code)]
    ts: Instant,
    m: MemStats,
}

static stats: ArcSwapOption<GlobalMstats> = ArcSwapOption::const_empty();

/// The cached heap statistics, read once if never read (Go `ReadMemStats`).
// go: ReadMemStats L1
pub fn read_mem_stats() -> MemStats {
    match stats.load().as_ref() {
        Some(s) => s.m,
        None => force_read_mem_stats(),
    }
}

/// Reads the heap statistics now and caches them (Go `ForceReadMemStats`).
// go: ForceReadMemStats L2 (runtime.ReadMemStats → jemalloc stats, A1 3.2)
pub fn force_read_mem_stats() -> MemStats {
    let m = jemalloc_stats();
    stats.store(Some(Arc::new(GlobalMstats {
        ts: Instant::now(),
        m,
    })));
    m
}

// glue: jemalloc statistics (refreshing the epoch first, as jemalloc requires)
fn jemalloc_stats() -> MemStats {
    use tikv_jemalloc_ctl::epoch;
    use tikv_jemalloc_ctl::stats as js;
    let _ = epoch::advance();
    let read = |r: tikv_jemalloc_ctl::Result<usize>| r.map(|v| v as u64).unwrap_or(0);
    MemStats {
        heap_alloc: read(js::allocated::read()),
        heap_inuse: read(js::active::read()),
        heap_sys: read(js::mapped::read()),
        sys: read(js::resident::read()),
    }
}
