//! go-file: pkg/table/index.go
//!
//! The index interface (block D06.4 part 1). The rest of the file is the write path
//! (`CreateIdxOpt` and its options, `IndexKVGenerator`, `IndexIterator`): permanent
//! exemption "write path".

use std::sync::Arc;

use tidb_model::{IndexInfo, TableInfo};

/// Index is the interface for index data on KV store.
///
/// The read half of Go's interface. `Create`, `Delete`, `GenIndexKVIter`, `Exist`,
/// `GenIndexValue`, `MeetPartialCondition*` are the write path; `GenIndexKey` and
/// `FetchValues` come with the index read paths (part 2).
pub trait Index: Send + Sync {
    /// Meta returns IndexInfo.
    // glue: interface method Index.Meta
    fn meta(&self) -> &IndexInfo;
    /// TableMeta returns TableInfo
    // glue: interface method Index.TableMeta
    fn table_meta(&self) -> &Arc<TableInfo>;
}

/// A shared `table.Index` interface value.
pub type IndexRef = Arc<dyn Index>;
