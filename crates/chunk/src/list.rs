//! go-file: pkg/util/chunk/list.go
//!
//! `List` (N5a 3.1: L1). Go's `memTracker.Consume` panics when an action fails; the port's
//! `Tracker::consume` returns the error (N3 3.4), so the methods that consume return
//! `Result`.

// Go names for package-level constants (mapping rule M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use tidb_base::Error;
use tidb_memory::Tracker;
use tidb_memory::tracker::LabelForChunkList;
use tidb_memory::tracker::new_tracker;
use tidb_parser::types::FieldType;

use crate::chunk::Chunk;
use crate::chunk::new;
use crate::chunk::renew;
use crate::row::Row;

/// List holds a slice of chunks, use to append rows with max chunk size properly handled.
#[derive(Debug)]
pub struct List {
    field_types: Vec<FieldType>,
    init_chunk_size: usize,
    max_chunk_size: usize,
    length: usize,
    chunks: Vec<Chunk>,
    freelist: Vec<Chunk>,

    /// track memory usage.
    mem_tracker: Arc<Tracker>,
    /// chunk index in "chunks", has been consumed.
    consumed_idx: isize,
}

/// RowPtrSize shows the size of RowPtr
pub const RowPtrSize: usize = std::mem::size_of::<RowPtr>();

/// RowPtr is used to get a row from a list.
/// It is only valid for the list that returns it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RowPtr {
    /// The chunk index.
    pub chk_idx: u32,
    /// The row index in the chunk.
    pub row_idx: u32,
}

/// NewListWithMemTracker creates a new List with field types, init chunk size, max chunk size and memory tracker.
// go: NewListWithMemTracker L1
pub fn new_list_with_mem_tracker(field_types: Vec<FieldType>, init_chunk_size: usize, max_chunk_size: usize, tracker: Arc<Tracker>) -> List {
    List {
        field_types,
        init_chunk_size,
        max_chunk_size,
        length: 0,
        chunks: Vec::new(),
        freelist: Vec::new(),
        mem_tracker: tracker,
        consumed_idx: -1,
    }
}

/// NewList creates a new List with field types, init chunk size and max chunk size.
// go: NewList L1
pub fn new_list(field_types: Vec<FieldType>, init_chunk_size: usize, max_chunk_size: usize) -> List {
    new_list_with_mem_tracker(field_types, init_chunk_size, max_chunk_size, new_tracker(LabelForChunkList, -1))
}

impl List {
    /// GetMemTracker returns the memory tracker of this List.
    // go: List.GetMemTracker L1
    pub fn get_mem_tracker(&self) -> &Arc<Tracker> {
        &self.mem_tracker
    }

    /// Len returns the length of the List.
    // go: List.Len L1
    pub fn len(&self) -> usize {
        self.length
    }

    /// Whether the list has no rows.
    // glue: Rust is_empty convention
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// NumChunks returns the number of chunks in the List.
    // go: List.NumChunks L1
    pub fn num_chunks(&self) -> usize {
        self.chunks.len()
    }

    /// FieldTypes returns the fieldTypes of the list
    // go: List.FieldTypes L1
    pub fn field_types(&self) -> &[FieldType] {
        &self.field_types
    }

    /// NumRowsOfChunk returns the number of rows of a chunk in the DataInDiskByRows.
    // go: List.NumRowsOfChunk L1
    pub fn num_rows_of_chunk(&self, chk_id: usize) -> usize {
        self.chunks[chk_id].num_rows()
    }

    /// GetChunk gets the Chunk by ChkIdx.
    // go: List.GetChunk L1
    pub fn get_chunk(&self, chk_idx: usize) -> &Chunk {
        &self.chunks[chk_idx]
    }

    /// AppendRow appends a row to the List, the row is copied to the List.
    // go: List.AppendRow L1
    pub fn append_row(&mut self, row: Row<'_>) -> Result<RowPtr, Error> {
        let mut chk_idx = self.chunks.len() as isize - 1;
        if chk_idx == -1
            || self.chunks[chk_idx as usize].num_rows() >= self.chunks[chk_idx as usize].capacity()
            || chk_idx == self.consumed_idx
        {
            let new_chk = self.alloc_chunk()?;
            self.chunks.push(new_chk);
            if chk_idx != self.consumed_idx {
                self.mem_tracker.consume(self.chunks[chk_idx as usize].memory_usage())?;
                self.consumed_idx = chk_idx;
            }
            chk_idx += 1;
        }
        let chk = &mut self.chunks[chk_idx as usize];
        let row_idx = chk.num_rows();
        chk.append_row(row);
        self.length += 1;
        Ok(RowPtr {
            chk_idx: chk_idx as u32,
            row_idx: row_idx as u32,
        })
    }

    /// Add adds a chunk to the List, the chunk may be modified later by the list.
    /// Caller must make sure the input chk is not empty and not used any more and has the same field types.
    // go: List.Add L1
    pub fn add(&mut self, chk: Chunk) -> Result<(), Error> {
        // FixMe: we should avoid add a Chunk that chk.NumRows() > list.maxChunkSize.
        if chk.num_rows() == 0 {
            // TODO: return error here.
            panic!("chunk appended to List should have at least 1 row");
        }
        let chk_idx = self.chunks.len() as isize - 1;
        if self.consumed_idx != chk_idx {
            self.mem_tracker.consume(self.chunks[chk_idx as usize].memory_usage())?;
            self.consumed_idx = chk_idx;
        }
        self.mem_tracker.consume(chk.memory_usage())?;
        self.consumed_idx += 1;
        self.length += chk.num_rows();
        self.chunks.push(chk);
        Ok(())
    }

    /// AllocChunk allocates a chunk from the freelist or creates a new one.
    // go: List.AllocChunk L1
    pub fn alloc_chunk(&mut self) -> Result<Chunk, Error> {
        if let Some(mut chk) = self.freelist.pop() {
            self.mem_tracker.consume(-chk.memory_usage())?;
            chk.reset();
            return Ok(chk);
        }
        if let Some(last) = self.chunks.last() {
            return Ok(renew(last, self.max_chunk_size));
        }
        Ok(new(&self.field_types, self.init_chunk_size, self.max_chunk_size))
    }

    /// GetRow gets a Row from the list by RowPtr.
    // go: List.GetRow L1
    #[inline]
    pub fn get_row(&self, ptr: RowPtr) -> Row<'_> {
        let chk = &self.chunks[ptr.chk_idx as usize];
        chk.get_row(ptr.row_idx as usize)
    }

    /// Reset resets the List.
    // go: List.Reset L1
    pub fn reset(&mut self) -> Result<(), Error> {
        let last_idx = self.chunks.len() as isize - 1;
        if last_idx != self.consumed_idx {
            self.mem_tracker.consume(self.chunks[last_idx as usize].memory_usage())?;
        }
        self.freelist.append(&mut self.chunks);
        self.length = 0;
        self.consumed_idx = -1;
        Ok(())
    }

    /// Clear triggers GC for all the allocated chunks and reset the list
    // go: List.Clear L1
    pub fn clear(&mut self) -> Result<(), Error> {
        self.mem_tracker.consume(-self.mem_tracker.bytes_consumed())?;
        self.freelist = Vec::new();
        self.chunks = Vec::new();
        self.length = 0;
        self.consumed_idx = -1;
        Ok(())
    }

    /// Walk iterate the list and call walkFunc for each row.
    // go: List.Walk L1
    pub fn walk(&self, mut walk_func: impl FnMut(Row<'_>) -> Result<(), Error>) -> Result<(), Error> {
        for chk in &self.chunks {
            for j in 0..chk.num_rows() {
                walk_func(chk.get_row(j))?;
            }
        }
        Ok(())
    }
}
