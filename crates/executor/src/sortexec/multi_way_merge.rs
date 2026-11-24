//! go-file: pkg/executor/sortexec/multi_way_merge.go
//!
//! The multi-way merge (k-way merge with a min-heap) of sorted row sources: the sorted rows
//! of the parallel sort's workers in memory ([`MemorySource`]), the sorted rows spilled to
//! disk ([`DiskSource`]) and the sort partitions of the unparallel sort
//! ([`SortPartitionSource`]).
//!
//! Go's sources hand out `chunk.Row`s that point into chunks the sources keep; here a row is a
//! [`RowHandle`] that holds its chunk (mapping rule M-ROW), `None` for Go's empty `Row{}`.
//! The disk source reads chunks back from the disk container (D01.10, not ported): it never
//! has data (see [`DataInDiskByChunks`]).

use std::sync::Arc;

use tidb_base::Error;
use tidb_chunk::row::{Row, RowHandle};

use super::heap;
use super::sort::SortKeys;
use super::sort_partition::SortPartition;
use super::sort_util::{
    DataCursor, DataInDiskByChunks, RowWithPartition, SharedSortedRowsIter, new_data_cursor,
    reload_cursor,
};

/// `multiWayMergeSource`: where the merge reads its sorted rows from, one sorted sequence per
/// partition.
pub trait MultiWayMergeSource: Send {
    /// Puts the first row of each partition into the heap and builds it.
    // glue: Go interface method multiWayMergeSource.init
    fn init(&mut self, multi_way_merge: &mut MultiWayMergeImpl) -> Result<(), Error>;
    /// The next row of the partition, `None` at its end.
    // glue: Go interface method multiWayMergeSource.next
    fn next(&mut self, partition_id: usize) -> Result<Option<RowHandle>, Error>;
    // glue: Go interface method multiWayMergeSource.getPartitionNum
    fn get_partition_num(&self) -> usize;
}

/// `memorySource`: the sorted rows of the parallel sort's workers, one iterator per worker,
/// shared with the worker that fills it (see [`SharedSortedRowsIter`]).
pub struct MemorySource {
    pub sorted_rows_iters: Vec<SharedSortedRowsIter>,
}

impl MultiWayMergeSource for MemorySource {
    // go: memorySource.init L1
    fn init(&mut self, multi_way_merge: &mut MultiWayMergeImpl) -> Result<(), Error> {
        for i in 0..self.sorted_rows_iters.len() {
            let row = self.sorted_rows_iters[i].lock().begin();
            let Some(row) = row else {
                continue;
            };
            multi_way_merge.elements.push(RowWithPartition {
                row: Some(row),
                partition_id: i,
            });
        }
        heap::init(multi_way_merge);
        Ok(())
    }

    // go: memorySource.next L1
    fn next(&mut self, partition_id: usize) -> Result<Option<RowHandle>, Error> {
        Ok(self.sorted_rows_iters[partition_id].lock().next())
    }

    // go: memorySource.getPartitionNum L1
    fn get_partition_num(&self) -> usize {
        self.sorted_rows_iters.len()
    }
}

/// `diskSource`: the sorted rows the parallel sort spilled, one container per spill.
///
/// The containers are shared with the spill helper that wrote them (Go shares the pointers).
pub struct DiskSource {
    pub sorted_rows_in_disk: Vec<Arc<DataInDiskByChunks>>,
    pub cursors: Vec<DataCursor>,
}

impl MultiWayMergeSource for DiskSource {
    // go: diskSource.init L2 (the cursor lends its row: the row is taken again with last)
    fn init(&mut self, multi_way_merge: &mut MultiWayMergeImpl) -> Result<(), Error> {
        let in_disk_num = self.sorted_rows_in_disk.len();
        self.cursors = Vec::with_capacity(in_disk_num);
        for i in 0..in_disk_num {
            let chk = self.sorted_rows_in_disk[i].get_chunk(0)?;
            let mut cursor = new_data_cursor();
            cursor.set_chunk(chk, 0);
            cursor.begin();
            let row = cursor.last();
            self.cursors.push(cursor);
            let Some(row) = row else {
                continue;
            };
            multi_way_merge.elements.push(RowWithPartition {
                row: Some(row),
                partition_id: i,
            });
        }
        heap::init(multi_way_merge);
        Ok(())
    }

    // go: diskSource.next L2 (the cursor lends its row: the row is taken again with last)
    fn next(&mut self, partition_id: usize) -> Result<Option<RowHandle>, Error> {
        self.cursors[partition_id].next();
        let mut new_row = self.cursors[partition_id].last();
        if new_row.is_none() {
            // Try to fetch more data from the disk
            let success = reload_cursor(
                &mut self.cursors[partition_id],
                Some(self.sorted_rows_in_disk[partition_id].as_ref()),
            )?;

            if !success {
                return Ok(None);
            }

            // Get new row
            self.cursors[partition_id].begin();
            new_row = self.cursors[partition_id].last();
            if new_row.is_none() {
                return Err(tidb_base::errors::new("Get an empty row"));
            }
        }
        Ok(new_row)
    }

    // go: diskSource.getPartitionNum L1
    fn get_partition_num(&self) -> usize {
        self.sorted_rows_in_disk.len()
    }
}

/// `sortPartitionSource`: the sort partitions of the unparallel sort.
///
/// It owns the partitions (Go shares the pointers with `e.Unparallel.sortPartitions`); its
/// only user, the merge of spilled partitions (`externalSorting`), is a boundary stub (D01.10).
pub struct SortPartitionSource {
    pub sort_partitions: Vec<SortPartition>,
}

impl MultiWayMergeSource for SortPartitionSource {
    // go: sortPartitionSource.init L1
    fn init(&mut self, multi_way_merge: &mut MultiWayMergeImpl) -> Result<(), Error> {
        let partition_num = self.get_partition_num();
        for i in 0..partition_num {
            let row = self.sort_partitions[i].get_next_sorted_row()?;

            multi_way_merge.elements.push(RowWithPartition {
                row,
                partition_id: i,
            });
        }
        heap::init(multi_way_merge);
        Ok(())
    }

    // go: sortPartitionSource.next L1
    fn next(&mut self, partition_id: usize) -> Result<Option<RowHandle>, Error> {
        self.sort_partitions[partition_id].get_next_sorted_row()
    }

    // go: sortPartitionSource.getPartitionNum L1
    fn get_partition_num(&self) -> usize {
        self.sort_partitions.len()
    }
}

/// `multiWayMergeImpl`: the heap of the merge, one element per partition that still has rows,
/// ordered by `lessRowFunction` (Go's `e.lessRow`, here the sort keys it reads).
pub struct MultiWayMergeImpl {
    pub less_row_function: SortKeys,
    pub elements: Vec<RowWithPartition>,
}

/// The row of a heap element; `None` (Go's empty `Row{}`) is the empty row.
// glue: Option<RowHandle> as Go's chunk.Row value
fn element_row(row: &Option<RowHandle>) -> Row<'_> {
    match row {
        Some(r) => r.row(),
        None => Row::default(),
    }
}

impl heap::Interface for MultiWayMergeImpl {
    // go: multiWayMergeImpl.Len L1
    fn len(&self) -> usize {
        self.elements.len()
    }

    // go: multiWayMergeImpl.Less L1
    fn less(&self, i: usize, j: usize) -> bool {
        let row_i = element_row(&self.elements[i].row);
        let row_j = element_row(&self.elements[j].row);
        let ret = self.less_row_function.less_row(row_i, row_j);
        ret < 0
    }

    // go: multiWayMergeImpl.Swap L1
    fn swap(&mut self, i: usize, j: usize) {
        self.elements.swap(i, j);
    }

    // go: multiWayMergeImpl.Pop L1
    fn pop(&mut self) {
        self.elements.pop();
    }
}

impl MultiWayMergeImpl {
    /// `Push` of `heap.Interface`: should never be called (no heap function the merge uses
    /// calls it).
    // go: multiWayMergeImpl.Push L1
    pub fn push(&mut self, _x: RowWithPartition) {
        // Should never be called.
    }
}

/// `multiWayMerger`: merges the sorted partitions of `source` into one sorted sequence.
pub struct MultiWayMerger<S: MultiWayMergeSource> {
    pub source: S,
    pub multi_way_merge: MultiWayMergeImpl,
}

// go: newMultiWayMerger L1
pub fn new_multi_way_merger<S: MultiWayMergeSource>(
    source: S,
    less_row_function: SortKeys,
) -> MultiWayMerger<S> {
    let partition_num = source.get_partition_num();
    MultiWayMerger {
        source,
        multi_way_merge: MultiWayMergeImpl {
            less_row_function,
            elements: Vec::with_capacity(partition_num),
        },
    }
}

impl<S: MultiWayMergeSource> MultiWayMerger<S> {
    // go: multiWayMerger.init L1
    pub fn init(&mut self) -> Result<(), Error> {
        self.source.init(&mut self.multi_way_merge)
    }

    /// The next row of the merged sequence, `None` when every partition is exhausted.
    // go: multiWayMerger.next L2 (the returned row is moved out of the heap element instead of copied, and put back on error)
    #[allow(clippy::should_implement_trait)] // Go's method name; it returns a Result
    pub fn next(&mut self) -> Result<Option<RowHandle>, Error> {
        if heap::Interface::len(&self.multi_way_merge) > 0 {
            let partition_id = self.multi_way_merge.elements[0].partition_id;
            let row = self.multi_way_merge.elements[0].row.take();
            let new_row = match self.source.next(partition_id) {
                Ok(r) => r,
                Err(err) => {
                    self.multi_way_merge.elements[0].row = row;
                    return Err(err);
                }
            };

            if new_row.is_none() {
                heap::remove(&mut self.multi_way_merge, 0);
                return Ok(row);
            }
            self.multi_way_merge.elements[0].row = new_row;
            heap::fix(&mut self.multi_way_merge, 0);
            return Ok(row);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use tidb_base::mysql::TypeLonglong;
    use tidb_chunk::chunk::{self, Chunk};
    use tidb_chunk::compare::get_compare_func;
    use tidb_types::field_type::new_field_type;

    use crate::sortexec::sort_util::new_sorted_rows_iter;

    fn int_chunk(vals: &[i64]) -> Arc<Chunk> {
        let ft = new_field_type(TypeLonglong);
        let mut chk = chunk::new(&[&ft], vals.len(), vals.len().max(1));
        for &v in vals {
            chk.append_int64(0, v);
        }
        Arc::new(chk)
    }

    fn keys(desc: bool) -> SortKeys {
        SortKeys {
            key_columns: Arc::from(vec![0usize]),
            key_cmp_funcs: Arc::from(vec![
                get_compare_func(&new_field_type(TypeLonglong)).unwrap(),
            ]),
            descs: Arc::from(vec![desc]),
        }
    }

    fn source(parts: &[&[i64]]) -> MemorySource {
        MemorySource {
            sorted_rows_iters: parts
                .iter()
                .map(|vals| {
                    let chk = int_chunk(vals);
                    let rows = (0..vals.len())
                        .map(|i| RowHandle::new(Arc::clone(&chk), i))
                        .collect();
                    Arc::new(Mutex::new(new_sorted_rows_iter(rows)))
                })
                .collect(),
        }
    }

    fn drain<S: MultiWayMergeSource>(m: &mut MultiWayMerger<S>) -> Vec<i64> {
        let mut out = Vec::new();
        while let Some(r) = m.next().unwrap() {
            out.push(r.row().get_int64(0));
        }
        out
    }

    #[test]
    fn merges_sorted_partitions() {
        let mut m = new_multi_way_merger(
            source(&[&[1, 4, 9], &[], &[2, 2, 3, 10], &[0], &[5, 6, 7, 8]]),
            keys(false),
        );
        assert_eq!(m.multi_way_merge.elements.capacity(), 5);
        m.init().unwrap();
        // The empty partition has no element.
        assert_eq!(m.multi_way_merge.elements.len(), 4);
        assert_eq!(drain(&mut m), vec![0, 1, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert!(m.next().unwrap().is_none());

        let mut m = new_multi_way_merger(source(&[&[9, 3, 1], &[8, 2]]), keys(true));
        m.init().unwrap();
        assert_eq!(drain(&mut m), vec![9, 8, 3, 2, 1]);

        let mut m = new_multi_way_merger(source(&[]), keys(false));
        m.init().unwrap();
        assert!(m.next().unwrap().is_none());
    }

    /// A source whose `next` fails: the merge returns the error and keeps its heap.
    struct Failing(MemorySource, bool);

    impl MultiWayMergeSource for Failing {
        fn init(&mut self, m: &mut MultiWayMergeImpl) -> Result<(), Error> {
            self.0.init(m)
        }
        fn next(&mut self, p: usize) -> Result<Option<RowHandle>, Error> {
            if self.1 {
                self.1 = false;
                return Err(Error::plain("read failed"));
            }
            self.0.next(p)
        }
        fn get_partition_num(&self) -> usize {
            self.0.get_partition_num()
        }
    }

    #[test]
    fn source_error_keeps_the_heap() {
        let mut m = new_multi_way_merger(Failing(source(&[&[1, 3], &[2]]), true), keys(false));
        m.init().unwrap();
        assert_eq!(m.next().unwrap_err().to_string(), "read failed");
        assert_eq!(drain(&mut m), vec![1, 2, 3]);
    }
}
