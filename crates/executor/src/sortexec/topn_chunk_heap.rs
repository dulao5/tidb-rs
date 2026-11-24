//! go-file: pkg/executor/sortexec/topn_chunk_heap.go
//!
//! `topNChunkHeap`: the rows of the top-n executor in a chunk list, and a max-heap of
//! pointers to them (Go's `heap.Interface`, here [`super::heap::Interface`]).
//!
//! Go keeps the executor's `compareRow` and `greaterRow` as function fields; here the heap
//! holds the sort keys those methods read ([`SortKeys`]). Go's `chunk.List` methods that
//! consume memory may panic with the quota or kill error; here they return it (A1), and so do
//! the heap methods that call them.

use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::Chunk;
use tidb_chunk::list::{List, RowPtr, RowPtrSize, new_list};
use tidb_chunk::row::Row;
use tidb_memory::tracker::{LabelForRowChunks, Tracker};
use tidb_parser::types::FieldType;

use super::heap::{self, Interface};
use super::sort::SortKeys;

/// topNChunkHeap implements heap.Interface.
#[derive(Default)]
pub struct TopNChunkHeap {
    /// `compareRow` and `greaterRow`: the sort keys they compare by.
    keys: Option<SortKeys>,

    /// rowChunks is the chunks to store row values.
    pub row_chunks: Option<List>,
    /// rowPointer store the chunk index and row index for each row.
    pub row_ptrs: Vec<RowPtr>,

    pub is_initialized: bool,
    pub is_row_ptrs_init: bool,

    pub mem_tracker: Option<Arc<Tracker>>,

    pub total_limit: u64,
    pub idx: isize,

    pub field_types: Vec<Arc<FieldType>>,
}

impl TopNChunkHeap {
    /// Go takes the executor for its return types, chunk sizes and `compareRow`, and the
    /// `greaterRow` to use; here their values are passed (`keys` for both comparisons).
    // go: topNChunkHeap.init L2 (the executor's values are passed; compareRow / greaterRow are the sort keys)
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        &mut self,
        ret_types: &[Arc<FieldType>],
        init_cap: isize,
        max_chunk_size: isize,
        keys: SortKeys,
        mem_tracker: &Arc<Tracker>,
        total_limit: u64,
        idx: isize,
        field_types: &[Arc<FieldType>],
    ) -> Result<(), Error> {
        self.mem_tracker = Some(Arc::clone(mem_tracker));

        let row_chunks = new_list(
            ret_types.iter().map(|ft| (**ft).clone()).collect(),
            init_cap as usize,
            max_chunk_size as usize,
        );
        row_chunks.get_mem_tracker().attach_to(mem_tracker)?;
        row_chunks.get_mem_tracker().set_label(LabelForRowChunks)?;
        self.row_chunks = Some(row_chunks);

        self.keys = Some(keys);

        self.total_limit = total_limit;
        self.idx = idx;
        self.is_initialized = true;

        self.field_types = field_types.to_vec();
        Ok(())
    }

    // go: topNChunkHeap.initPtrs L2 (the tracker's error is returned [M-MEM])
    pub fn init_ptrs(&mut self) -> Result<(), Error> {
        self.tracker()
            .consume((RowPtrSize as i64).wrapping_mul(self.chunks().len() as i64))?;
        self.init_ptrs_impl();
        Ok(())
    }

    // go: topNChunkHeap.initPtrsImpl L1
    pub fn init_ptrs_impl(&mut self) {
        let row_chunks = self.chunks();
        let mut row_ptrs = Vec::with_capacity(row_chunks.len());
        for chk_idx in 0..row_chunks.num_chunks() {
            let row_chk = row_chunks.get_chunk(chk_idx);
            for row_idx in 0..row_chk.num_rows() {
                row_ptrs.push(RowPtr {
                    chk_idx: chk_idx as u32,
                    row_idx: row_idx as u32,
                });
            }
        }
        self.row_ptrs = row_ptrs;
        self.is_row_ptrs_init = true;
    }

    // go: topNChunkHeap.clear L2 (the tracker's error is returned [M-MEM])
    pub fn clear(&mut self) -> Result<(), Error> {
        self.chunks_mut().clear()?;
        self.tracker()
            .consume(-(RowPtrSize as i64).wrapping_mul(self.row_ptrs.len() as i64))?;
        self.row_ptrs = Vec::new();
        self.is_row_ptrs_init = false;
        self.is_initialized = false;
        self.idx = 0;
        Ok(())
    }

    /// Takes the pointer of the heap's max row (Go takes the row, which borrows the list this
    /// call appends to).
    // go: topNChunkHeap.update L2 (the heap max row is passed by its pointer; the list's error is returned [M-ROW, M-MEM])
    pub fn update(&mut self, heap_max_row: RowPtr, new_row: Row<'_>) -> Result<(), Error> {
        let greater = {
            let row_chunks = self.chunks();
            self.sort_keys()
                .greater_row(row_chunks.get_row(heap_max_row), new_row)
        };
        if greater {
            // Evict heap max, keep the next row.
            self.row_ptrs[0] = self.chunks_mut().append_row(new_row)?;
            heap::fix(self, 0);
        }
        Ok(())
    }

    // go: topNChunkHeap.processChk L2 (the list's error is returned [M-MEM])
    pub fn process_chk(&mut self, chk: &Chunk) -> Result<(), Error> {
        for i in 0..chk.num_rows() {
            let heap_max_row = self.row_ptrs[0];
            let new_row = chk.get_row(i);
            self.update(heap_max_row, new_row)?;
        }
        Ok(())
    }

    /// doCompaction rebuild the chunks and row pointers to release memory. If we don't do
    /// compaction, in a extreme case like the child data is already ascending sorted but we
    /// want descending top N, then we will keep all data in memory. But if data is
    /// distributed randomly, this function will be called log(n) times.
    // go: topNChunkHeap.doCompaction L2 (the executor's return types and chunk sizes are passed; the trackers' errors are returned [M-MEM])
    pub fn do_compaction(
        &mut self,
        ret_types: &[Arc<FieldType>],
        init_cap: isize,
        max_chunk_size: isize,
    ) -> Result<(), Error> {
        let mut new_row_chunks = new_list(
            ret_types.iter().map(|ft| (**ft).clone()).collect(),
            init_cap as usize,
            max_chunk_size as usize,
        );
        let mut new_row_ptrs = Vec::with_capacity(self.chunks().len());
        for &row_ptr in &self.row_ptrs {
            let new_row_ptr = new_row_chunks.append_row(self.chunks().get_row(row_ptr))?;
            new_row_ptrs.push(new_row_ptr);
        }
        new_row_chunks
            .get_mem_tracker()
            .set_label(LabelForRowChunks)?;
        self.tracker().replace_child(
            self.chunks().get_mem_tracker(),
            Some(new_row_chunks.get_mem_tracker()),
        )?;
        self.row_chunks = Some(new_row_chunks);

        self.tracker()
            .consume((RowPtrSize as i64).wrapping_mul(
                (new_row_ptrs.len() as i64).wrapping_sub(self.row_ptrs.len() as i64),
            ))?;
        self.row_ptrs = new_row_ptrs;
        Ok(())
    }

    // go: topNChunkHeap.keyColumnsCompare L1
    pub fn key_columns_compare(&self, i: RowPtr, j: RowPtr) -> isize {
        let row_i = self.chunks().get_row(i);
        let row_j = self.chunks().get_row(j);
        self.sort_keys().compare_row(row_i, row_j)
    }

    // glue: h.rowChunks (a nil pointer dereference panics in Go)
    pub fn chunks(&self) -> &List {
        self.row_chunks
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: h.rowChunks (a nil pointer dereference panics in Go)
    pub fn chunks_mut(&mut self) -> &mut List {
        self.row_chunks
            .as_mut()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: h.memTracker (a nil pointer dereference panics in Go)
    fn tracker(&self) -> &Arc<Tracker> {
        self.mem_tracker
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }

    // glue: h.compareRow / h.greaterRow (nil functions panic in Go)
    fn sort_keys(&self) -> &SortKeys {
        self.keys
            .as_ref()
            .expect("invalid memory address or nil pointer dereference")
    }
}

impl Interface for TopNChunkHeap {
    // go: topNChunkHeap.Len L1
    fn len(&self) -> usize {
        self.row_ptrs.len()
    }

    /// Less implement heap.Interface, but since we mantains a max heap, this function returns
    /// true if row i is greater than row j.
    // go: topNChunkHeap.Less L1
    fn less(&self, i: usize, j: usize) -> bool {
        let row_i = self.chunks().get_row(self.row_ptrs[i]);
        let row_j = self.chunks().get_row(self.row_ptrs[j]);
        self.sort_keys().greater_row(row_i, row_j)
    }

    // go: topNChunkHeap.Swap L1
    fn swap(&mut self, i: usize, j: usize) {
        self.row_ptrs.swap(i, j);
    }

    /// We don't need the popped value (Go returns nil to avoid memory allocation).
    // go: topNChunkHeap.Pop L1
    fn pop(&mut self) {
        self.row_ptrs.truncate(self.row_ptrs.len() - 1);
    }
}

impl TopNChunkHeap {
    /// Push: should never be called.
    // go: topNChunkHeap.Push L1
    pub fn push(&mut self) {
        // Should never be called.
    }
}

/// TestKillSignalInTopN is for test: it spills a heap after a kill signal and expects the
/// kill error.
// glue: stub of TestKillSignalInTopN, blocked-by:D01.10 (the test helper spills the heap to disk)
pub fn test_kill_signal_in_top_n() -> Result<(), Error> {
    Err(blocked("D01.10", "spilling the top-n heap to disk"))
}
