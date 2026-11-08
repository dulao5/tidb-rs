//! go-file: pkg/util/chunk/alloc.go
//!
//! Chunk allocators. A1 removes the reuse pool (mapping rule `M-POOL`): every allocator
//! allocates new chunks (Go's `emptyAllocator`), and the pool internals (`allocator`,
//! `poolColumnAllocator`, `columnList`, `checkColumnType`, `pool.go`) are not ported (tracker
//! status X). The allocator types are kept so callers keep Go's shape; the session variable
//! `tidb_enable_reuse_chunk` is accepted and warns when not default (N8, A1 open item 3).

use parking_lot::Mutex;
use tidb_parser::types::FieldType;

use crate::chunk::Chunk;
use crate::chunk::new;

/// Allocator is an interface defined to reduce object allocation.
/// The typical usage is to call Reset() to recycle objects into a pool,
/// and Alloc() allocates from the pool.
pub trait Allocator: Send {
    /// Alloc allocates a chunk.
    // glue: Go interface method
    fn alloc(&mut self, fields: &[FieldType], capacity: usize, max_chunk_size: usize) -> Chunk;
    /// CheckReuseAllocSize reports whether the allocator would reuse memory.
    // glue: Go interface method
    fn check_reuse_alloc_size(&self) -> bool;
    /// Reset recycles the allocated chunks.
    // glue: Go interface method
    fn reset(&mut self);
}

/// InitChunkAllocSize init the maximum cache size
// go: InitChunkAllocSize L2 (M-POOL: no pool, the sizes are ignored)
pub fn init_chunk_alloc_size(_set_max_free_chunks: u32, _set_max_free_columns: u32) {}

/// NewAllocator creates an Allocator.
// go: NewAllocator L2 (M-POOL: an allocator that does not reuse, Go's emptyAllocator)
pub fn new_allocator() -> Box<dyn Allocator> {
    new_empty_allocator()
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
    fn alloc(&mut self, fields: &[FieldType], capacity: usize, max_chunk_size: usize) -> Chunk {
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
    fn alloc(&mut self, fields: &[FieldType], capacity: usize, max_chunk_size: usize) -> Chunk {
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
    fn alloc(&mut self, fields: &[FieldType], capacity: usize, max_chunk_size: usize) -> Chunk {
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
