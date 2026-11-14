//! go-file: pkg/util/chunk/alloc.go
//!
//! Chunk allocators. A1 removes the reuse pool (mapping rule `M-POOL`): every allocator
//! allocates new chunks (Go's `emptyAllocator`), and the pool internals (`allocator.Alloc`,
//! `allocator.Reset`, `poolColumnAllocator`, `columnList`, `checkColumnType`, `pool.go`) are
//! not ported (tracker status X). The allocator types are kept so callers keep Go's shape.
//!
//! `allocator.CheckReuseAllocSize` IS ported (D05.2 PR E): it only reads the configured pool
//! sizes, and its answer is visible to users. `SessionVars.SetAlloc` wraps the allocator in
//! a `reuseHookAllocator` whose hook marks the statement as having used the chunk allocator;
//! the hook fires on the first `Alloc` when `CheckReuseAllocSize` is true, and the result
//! is read by `SELECT @@last_sql_use_alloc`. So the no-reuse allocator returned by
//! `NewAllocator` still answers like Go's pool.

use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::Relaxed;

use parking_lot::Mutex;
use tidb_parser::types::FieldType;

use crate::chunk::Chunk;
use crate::chunk::new;

/// Allocator is an interface defined to reduce object allocation.
/// The typical usage is to call Reset() to recycle objects into a pool,
/// and Alloc() allocates from the pool.
pub trait Allocator: Send {
    /// Alloc allocates a chunk. `fields` is Go's `[]*types.FieldType`: the caller's shared
    /// types (an executor's return types, N13 §8).
    // glue: Go interface method
    fn alloc(&mut self, fields: &[Arc<FieldType>], capacity: usize, max_chunk_size: usize)
    -> Chunk;
    /// CheckReuseAllocSize reports whether the allocator would reuse memory.
    // glue: Go interface method
    fn check_reuse_alloc_size(&self) -> bool;
    /// Reset recycles the allocated chunks.
    // glue: Go interface method
    fn reset(&mut self);
}

#[allow(non_upper_case_globals)]
/// maxFreeChunks is the maximum number of free chunks the pool keeps (Go `var maxFreeChunks = 64`).
static maxFreeChunks: AtomicI64 = AtomicI64::new(64);
#[allow(non_upper_case_globals)]
/// maxFreeColumnsPerType is the maximum number of free columns per type the pool keeps
/// (Go `var maxFreeColumnsPerType = 256`).
static maxFreeColumnsPerType: AtomicI64 = AtomicI64::new(256);

/// InitChunkAllocSize init the maximum cache size
// M-POOL: there is no pool, but the sizes still decide CheckReuseAllocSize.
// go: InitChunkAllocSize L1
pub fn init_chunk_alloc_size(set_max_free_chunks: u32, set_max_free_columns: u32) {
    let set_max_free_chunks = set_max_free_chunks.min(i32::MAX as u32);
    let set_max_free_columns = set_max_free_columns.min(i32::MAX as u32);
    maxFreeChunks.store(i64::from(set_max_free_chunks), Relaxed);
    maxFreeColumnsPerType.store(i64::from(set_max_free_columns), Relaxed);
}

/// NewAllocator creates an Allocator.
// It keeps the pool sizes, so CheckReuseAllocSize answers like Go's pool.
// go: NewAllocator L2 (M-POOL: allocates new chunks like emptyAllocator)
pub fn new_allocator() -> Box<dyn Allocator> {
    Box::new(PoolSizeAllocator {
        free_chunk: maxFreeChunks.load(Relaxed),
        // Go: columnAlloc.init() sets freeColumnsPerType = maxFreeColumnsPerType.
        free_columns_per_type: maxFreeColumnsPerType.load(Relaxed),
    })
}

/// allocator is Go's pooled allocator with the pool removed (M-POOL): only the two size
/// limits are kept, for `CheckReuseAllocSize`.
// glue: allocator (M-POOL; only freeChunk and columnAlloc.freeColumnsPerType kept)
#[derive(Clone, Copy, Debug)]
struct PoolSizeAllocator {
    free_chunk: i64,
    free_columns_per_type: i64,
}

impl Allocator for PoolSizeAllocator {
    /// Alloc allocates a new chunk (M-POOL: never from a free list).
    // glue: allocator.Alloc is X (M-POOL); allocates like emptyAllocator.Alloc
    fn alloc(
        &mut self,
        fields: &[Arc<FieldType>],
        capacity: usize,
        max_chunk_size: usize,
    ) -> Chunk {
        new(fields, capacity, max_chunk_size)
    }

    /// CheckReuseAllocSize check whether the allocator can reuse memory
    // go: allocator.CheckReuseAllocSize L1
    fn check_reuse_alloc_size(&self) -> bool {
        self.free_chunk > 0 || self.free_columns_per_type > 0
    }

    /// Reset recycles nothing (M-POOL).
    // glue: allocator.Reset is X (M-POOL)
    fn reset(&mut self) {}
}

/// syncAllocator makes an allocator safe for concurrent use.
struct SyncAllocator {
    alloc: Mutex<Box<dyn Allocator>>,
}

/// NewSyncAllocator creates the synchronized version of the `alloc`
// go: NewSyncAllocator L1
pub fn new_sync_allocator(alloc: Box<dyn Allocator>) -> Box<dyn Allocator> {
    Box::new(SyncAllocator {
        alloc: Mutex::new(alloc),
    })
}

impl Allocator for SyncAllocator {
    /// Alloc implements `Allocator` for `*syncAllocator`
    // go: syncAllocator.Alloc L1
    fn alloc(
        &mut self,
        fields: &[Arc<FieldType>],
        capacity: usize,
        max_chunk_size: usize,
    ) -> Chunk {
        self.alloc.lock().alloc(fields, capacity, max_chunk_size)
    }

    /// CheckReuseAllocSize implements `Allocator` for `*syncAllocator`
    // go: syncAllocator.CheckReuseAllocSize L1
    fn check_reuse_alloc_size(&self) -> bool {
        self.alloc.lock().check_reuse_alloc_size()
    }

    /// Reset implements `Allocator` for `*syncAllocator`
    // go: syncAllocator.Reset L1
    fn reset(&mut self) {
        self.alloc.lock().reset();
    }
}

/// reuseHookAllocator will run the function hook when it allocates the first chunk from reused part
struct ReuseHookAllocator {
    once: bool,
    f: Box<dyn FnMut() + Send>,
    alloc: Box<dyn Allocator>,
}

/// NewReuseHookAllocator creates an allocator, which will call the function `f` when the first reused chunk is allocated.
// go: NewReuseHookAllocator L1
pub fn new_reuse_hook_allocator(
    alloc: Box<dyn Allocator>,
    f: Box<dyn FnMut() + Send>,
) -> Box<dyn Allocator> {
    Box::new(ReuseHookAllocator {
        once: false,
        f,
        alloc,
    })
}

impl Allocator for ReuseHookAllocator {
    /// Alloc implements the Allocator interface
    // go: reuseHookAllocator.Alloc L1
    fn alloc(
        &mut self,
        fields: &[Arc<FieldType>],
        capacity: usize,
        max_chunk_size: usize,
    ) -> Chunk {
        if self.alloc.check_reuse_alloc_size() && !self.once {
            self.once = true;
            (self.f)();
        }
        self.alloc.alloc(fields, capacity, max_chunk_size)
    }

    /// CheckReuseAllocSize implements the Allocator interface
    // go: reuseHookAllocator.CheckReuseAllocSize L1
    fn check_reuse_alloc_size(&self) -> bool {
        self.alloc.check_reuse_alloc_size()
    }

    /// Reset implements the Allocator interface
    // go: reuseHookAllocator.Reset L1
    fn reset(&mut self) {
        self.alloc.reset();
    }
}

/// emptyAllocator is an allocator which just allocates new chunks.
#[derive(Clone, Copy, Debug, Default)]
struct EmptyAllocator;

/// NewEmptyAllocator creates an empty pool, which will always call `chunk.New` to create a new chunk
// go: NewEmptyAllocator L1
pub fn new_empty_allocator() -> Box<dyn Allocator> {
    Box::new(EmptyAllocator)
}

impl Allocator for EmptyAllocator {
    /// Alloc implements the Allocator interface
    // go: emptyAllocator.Alloc L1
    fn alloc(
        &mut self,
        fields: &[Arc<FieldType>],
        capacity: usize,
        max_chunk_size: usize,
    ) -> Chunk {
        new(fields, capacity, max_chunk_size)
    }

    /// CheckReuseAllocSize implements the Allocator interface
    // go: emptyAllocator.CheckReuseAllocSize L1
    fn check_reuse_alloc_size(&self) -> bool {
        false
    }

    /// Reset implements the Allocator interface
    // go: emptyAllocator.Reset L1
    fn reset(&mut self) {}
}
