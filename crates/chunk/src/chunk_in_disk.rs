//! go-file: pkg/util/chunk/chunk_in_disk.go
//!
//! Temporary disk container for spilled chunks (block D01.10).
//!
//! Only the five non-I/O methods (`NewDataInDiskByChunks`, `GetDiskTracker`, `NumChunks`,
//! `NumRows`, `Close`) are ported ahead of D01.10 (decision 4 of
//! `specs/D12.3-agg-executors/spec.md` §8.2) so executors that unconditionally construct and
//! inspect an empty container on `Open` / `Close` work without disk I/O. The disk I/O
//! methods (`Add`, `GetChunk`) return `blocked-by:D01.10` until D01.10 lands.

// Go identifier for the exported path prefix constant (M-GONAME).
#![allow(non_upper_case_globals)]

use std::borrow::Borrow;
use std::sync::Arc;

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_memory::tracker::{LabelForChunkDataInDiskByChunks, Tracker, new_tracker};
use tidb_parser::types::FieldType;

use crate::chunk::Chunk;

/// DefaultChunkDataInDiskByChunksPath gives the file name prefix.
pub const DefaultChunkDataInDiskByChunksPath: &str = "defaultChunkDataInDiskByChunksPath";

/// DataInDiskByChunks represents some data stored in temporary disk.
/// They can only be restored by chunks.
#[derive(Debug)]
pub struct DataInDiskByChunks {
    #[allow(dead_code)] // Read when D01.10 implements GetChunk.
    field_types: Vec<FieldType>,
    offset_of_each_chunk: Vec<i64>,
    #[allow(dead_code)] // Updated when D01.10 implements Add.
    total_data_size: i64,
    total_row_num: i64,
    disk_tracker: Arc<Tracker>,
    #[allow(dead_code)] // Used when D01.10 implements initDiskFile.
    file_name_prefix_for_test: String,
}

/// NewDataInDiskByChunks creates a new DataInDiskByChunks with field types.
// go: NewDataInDiskByChunks L1
pub fn new_data_in_disk_by_chunks<F: Borrow<FieldType>>(
    field_types: &[F],
    file_name_prefix_for_test: String,
) -> DataInDiskByChunks {
    DataInDiskByChunks {
        field_types: field_types.iter().map(|ft| ft.borrow().clone()).collect(),
        offset_of_each_chunk: Vec::new(),
        total_data_size: 0,
        total_row_num: 0,
        // TODO: set the quota of disk usage.
        disk_tracker: new_tracker(LabelForChunkDataInDiskByChunks, -1),
        file_name_prefix_for_test,
    }
}

impl DataInDiskByChunks {
    /// GetDiskTracker returns the disk tracker of this container.
    // go: DataInDiskByChunks.GetDiskTracker L1
    pub fn get_disk_tracker(&self) -> &Arc<Tracker> {
        &self.disk_tracker
    }

    /// Add adds a chunk to the DataInDiskByChunks.
    // glue: stub of DataInDiskByChunks.Add, blocked-by:D01.10 (spilling chunks to disk)
    pub fn add(&self, _chk: &Chunk) -> Result<(), Error> {
        Err(blocked(
            "D01.10",
            "spilling chunks to disk (DataInDiskByChunks.Add)",
        ))
    }

    /// GetChunk gets a Chunk from the DataInDiskByChunks by chkIdx.
    // glue: stub of DataInDiskByChunks.GetChunk, blocked-by:D01.10 (reading spilled chunks from disk)
    pub fn get_chunk(&self, _chk_idx: usize) -> Result<Chunk, Error> {
        Err(blocked(
            "D01.10",
            "reading spilled chunks from disk (DataInDiskByChunks.GetChunk)",
        ))
    }

    /// Close releases the disk resource.
    // go: DataInDiskByChunks.Close L2 (no disk file is opened before D01.10; releases tracked disk bytes if any)
    pub fn close(&self) {
        let consumed = self.disk_tracker.bytes_consumed();
        if consumed != 0 {
            let _ = self.disk_tracker.consume(-consumed);
        }
    }

    /// NumRows returns total spilled row number.
    // go: DataInDiskByChunks.NumRows L1
    pub fn num_rows(&self) -> i64 {
        self.total_row_num
    }

    /// NumChunks returns total spilled chunk number.
    // go: DataInDiskByChunks.NumChunks L1
    pub fn num_chunks(&self) -> usize {
        self.offset_of_each_chunk.len()
    }
}

#[cfg(test)]
mod tests {
    use tidb_base::blocked::blocked_by;
    use tidb_base::mysql::TypeLonglong;
    use tidb_types::field_type::new_field_type;

    use super::*;
    use crate::chunk::new as new_chunk;

    #[test]
    fn empty_container_non_io_methods_and_io_stubs() {
        let ft = new_field_type(TypeLonglong);
        let d = new_data_in_disk_by_chunks(&[&ft], "test_prefix".to_string());
        assert_eq!(d.num_chunks(), 0);
        assert_eq!(d.num_rows(), 0);
        assert_eq!(
            d.get_disk_tracker().label(),
            LabelForChunkDataInDiskByChunks
        );
        assert_eq!(d.get_disk_tracker().bytes_consumed(), 0);

        let chk = new_chunk(&[&ft], 1, 1);
        assert_eq!(blocked_by(&d.add(&chk).unwrap_err()), Some("D01.10"));
        assert_eq!(blocked_by(&d.get_chunk(0).unwrap_err()), Some("D01.10"));

        d.close();
        assert_eq!(d.get_disk_tracker().bytes_consumed(), 0);
    }
}
