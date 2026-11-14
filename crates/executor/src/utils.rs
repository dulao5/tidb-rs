//! go-file: pkg/executor/utils.go
//!
//! Small helpers of the executor package: string sets (privilege lists), the DML child chunk
//! size, the batch retriever of memory tables, and the password encoding of `CREATE USER`.

// Go identifiers for the constants (M-GONAME).
#![allow(non_upper_case_globals)]

use tidb_base::Error;
use tidb_base::blocked::blocked;
use tidb_chunk::chunk::{Chunk, ZeroCapacity};
use tidb_parser::types::FieldType;

use crate::internal::exec::executor::Executor;

/// SetFromString constructs a slice of strings from a comma separated string.
/// It is assumed that there is no duplicated entry. You could use addToSet to maintain this
/// property. It is exported for tests. I HOPE YOU KNOW WHAT YOU ARE DOING.
// go: SetFromString L1
pub fn set_from_string(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    value.split(',').map(str::to_owned).collect()
}

// go: setToString L1
pub fn set_to_string(set: &[String]) -> String {
    set.join(",")
}

/// addToSet add a value to the set, e.g:
/// addToSet("Select,Insert,Update", "Update") returns "Select,Insert,Update".
// go: addToSet L1
pub fn add_to_set(mut set: Vec<String>, value: &str) -> Vec<String> {
    if set.iter().any(|v| v == value) {
        return set;
    }
    set.push(value.to_owned());
    set
}

/// deleteFromSet delete the value from the set, e.g:
/// deleteFromSet("Select,Insert,Update", "Update") returns "Select,Insert".
// go: deleteFromSet L1
pub fn delete_from_set(mut set: Vec<String>, value: &str) -> Vec<String> {
    if let Some(i) = set.iter().position(|v| v == value) {
        set.remove(i);
    }
    set
}

const dmlChildChunkTargetBytes: isize = 256 * 1024;

/// Use child InitCap as the upper bound, but reduce it for wide rows to avoid large upfront
/// allocation.
// go: newDMLChildChunk L1
pub fn new_dml_child_chunk(
    dml_exec: &dyn Executor,
    fields: &[FieldType],
    max_init_cap: isize,
) -> Chunk {
    let max_chunk_size = dml_exec.max_chunk_size();
    let init_cap = estimate_dml_child_chunk_init_cap(fields, max_chunk_size, max_init_cap);
    dml_exec.new_chunk_with_capacity(fields, init_cap, max_chunk_size)
}

// go: estimateDMLChildChunkInitCap L1
pub fn estimate_dml_child_chunk_init_cap(
    fields: &[FieldType],
    max_chunk_size: isize,
    max_init_cap: isize,
) -> isize {
    if max_chunk_size <= 0 || max_init_cap <= 0 {
        return ZeroCapacity as isize;
    }
    let mut row_width: isize = 0;
    for field in fields {
        row_width += tidb_chunk::codec::estimate_type_width(field);
    }
    if row_width <= 0 {
        return max_chunk_size.min(max_init_cap);
    }
    1.max(
        max_chunk_size
            .min(max_init_cap)
            .min(dmlChildChunkTargetBytes / row_width),
    )
}

/// batchRetrieverHelper is a helper for batch returning data with known total rows. This
/// helps implementing memtable retrievers of some information_schema tables. Initialize
/// `batchSize` and `totalRows` fields to use it.
#[derive(Clone, Debug, Default)]
pub struct BatchRetrieverHelper {
    /// When retrieved is true, it means retrieving is finished.
    pub retrieved: bool,
    /// The index that the retrieving process has been done up to (exclusive).
    pub retrieved_idx: isize,
    pub batch_size: isize,
    pub total_rows: isize,
}

impl BatchRetrieverHelper {
    /// nextBatch calculates the index range of the next batch. If there is such a non-empty
    /// range, the `retrieveRange` func will be invoked and the range [start, end) is passed to
    /// it. Returns error if `retrieveRange` returns error.
    // go: batchRetrieverHelper.nextBatch L1
    pub fn next_batch(
        &mut self,
        mut retrieve_range: impl FnMut(isize, isize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if self.retrieved_idx >= self.total_rows {
            self.retrieved = true;
        }
        if self.retrieved {
            return Ok(());
        }
        let start = self.retrieved_idx;
        let mut end = self.retrieved_idx + self.batch_size;
        if end > self.total_rows {
            end = self.total_rows;
        }

        if let Err(err) = retrieve_range(start, end) {
            self.retrieved = true;
            return Err(err);
        }
        self.retrieved_idx = end;
        if self.retrieved_idx == self.total_rows {
            self.retrieved = true;
        }
        Ok(())
    }
}

/// encodePassword encodes the password for the user. It invokes the auth plugin if it is
/// available.
///
/// `CREATE USER` / `ALTER USER` / `SET PASSWORD` are privilege statements (D04.1, with the
/// extension auth plugins): not on the read path.
// go: encodePassword L2 (boundary stub, blocked-by:D04.1)
pub fn encode_password() -> Result<(String, bool), Error> {
    Err(blocked("D04.1", "password encoding of user statements"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go utils_test.go TestBatchRetrieverHelper.
    #[test]
    fn batch_retriever_helper() {
        let mut starts: Vec<isize> = Vec::new();
        let mut ends: Vec<isize> = Vec::new();

        let mut r = BatchRetrieverHelper::default();
        r.next_batch(|s, e| {
            starts.push(s);
            ends.push(e);
            Ok(())
        })
        .unwrap();
        assert!(starts.is_empty() && ends.is_empty());

        let mut r = BatchRetrieverHelper {
            retrieved: true,
            batch_size: 3,
            total_rows: 10,
            ..Default::default()
        };
        r.next_batch(|s, e| {
            starts.push(s);
            ends.push(e);
            Ok(())
        })
        .unwrap();
        assert!(starts.is_empty() && ends.is_empty());

        let mut r = BatchRetrieverHelper {
            batch_size: 3,
            total_rows: 10,
            ..Default::default()
        };
        assert!(
            r.next_batch(|_, _| Err(Error::plain("some error")))
                .is_err()
        );
        assert!(r.retrieved);

        for (batch, total, want_s, want_e) in [
            (3, 10, vec![0, 3, 6, 9], vec![3, 6, 9, 10]),
            (3, 9, vec![0, 3, 6], vec![3, 6, 9]),
            (100, 10, vec![0], vec![10]),
        ] {
            starts.clear();
            ends.clear();
            let mut r = BatchRetrieverHelper {
                batch_size: batch,
                total_rows: total,
                ..Default::default()
            };
            while !r.retrieved {
                r.next_batch(|s, e| {
                    starts.push(s);
                    ends.push(e);
                    Ok(())
                })
                .unwrap();
            }
            assert_eq!(starts, want_s);
            assert_eq!(ends, want_e);
        }
    }

    #[test]
    fn string_sets() {
        assert!(set_from_string("").is_empty());
        let set = set_from_string("Select,Insert,Update");
        assert_eq!(set, vec!["Select", "Insert", "Update"]);
        let set = add_to_set(set, "Update");
        assert_eq!(set_to_string(&set), "Select,Insert,Update");
        let set = add_to_set(set, "Delete");
        assert_eq!(set_to_string(&set), "Select,Insert,Update,Delete");
        let set = delete_from_set(set, "Update");
        assert_eq!(set_to_string(&set), "Select,Insert,Delete");
        let set = delete_from_set(set, "None");
        assert_eq!(set_to_string(&set), "Select,Insert,Delete");
    }

    #[test]
    fn dml_child_chunk_init_cap() {
        use tidb_base::mysql::{TypeLonglong, TypeVarchar};
        use tidb_types::field_type::new_field_type;
        let ints = vec![new_field_type(TypeLonglong); 2];
        assert_eq!(estimate_dml_child_chunk_init_cap(&ints, 1024, 32), 32);
        assert_eq!(estimate_dml_child_chunk_init_cap(&ints, 0, 32), 0);
        assert_eq!(estimate_dml_child_chunk_init_cap(&[], 1024, 2048), 1024);
        let mut wide = new_field_type(TypeVarchar);
        wide.set_flen(1 << 20);
        let w = estimate_dml_child_chunk_init_cap(&[wide], 1024, 1024);
        assert!((1..1024).contains(&w), "{w}");
    }
}
