//! go-file: pkg/table/tables/index.go
//!
//! The `index` struct behind `table.Index` (block D06.4 part 1): construction and metadata.
//! Key encoding, `FetchValues` and the index helpers used by the index read paths are
//! part 2; `Create`, `Delete`, `Exist`, the temp-index and duplicate-key helpers and the
//! partial-index condition evaluation are the write path.

use std::sync::Arc;

use tidb_base::Error;
use tidb_model::{IndexInfo, TableInfo};

use crate::table::{Index, IndexRef};

/// index is the data structure for index data in the KV store.
///
/// Go also keeps the parsed partial-index condition (`indexPartialCondition`, built with
/// `expression.ParseSimpleExpr`) and a pool of evaluation buffers. Both are read only by
/// `MeetPartialCondition*` on the write path, so they are not built (see [`new_index`]).
pub struct IndexImpl {
    idx_info: Arc<IndexInfo>,
    tbl_info: Arc<TableInfo>,
    #[allow(dead_code)]
    phy_tbl_id: i64,
}

/// NewIndex builds a new Index object.
///
/// The partial-index condition (`ConditionExprString`) is parsed in Go only for the write
/// path (`MeetPartialCondition`); its `ParseSimpleExpr` is not called, so a condition that
/// does not parse is not reported here (D06.4 spec §4).
// go: NewIndex L2 (the partial-index condition, write path only, is not parsed)
pub fn new_index(
    physical_id: i64,
    tbl_info: &Arc<TableInfo>,
    index_info: Arc<IndexInfo>,
) -> Result<IndexRef, Error> {
    let index = IndexImpl {
        idx_info: index_info,
        tbl_info: Arc::clone(tbl_info),
        phy_tbl_id: physical_id,
    };
    Ok(Arc::new(index))
}

impl Index for IndexImpl {
    /// Meta returns index info.
    // go: index.Meta L1
    fn meta(&self) -> &Arc<IndexInfo> {
        &self.idx_info
    }

    /// TableMeta returns table info.
    // go: index.TableMeta L1
    fn table_meta(&self) -> &Arc<TableInfo> {
        &self.tbl_info
    }
}
