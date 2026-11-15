//! Small pieces of `pkg/planner/util` the pilot needs.

use std::sync::Arc;

use tidb_base::mysql::NotNullFlag;
use tidb_expression::{Column, ExprRef, Schema};

/// go-file: pkg/planner/util/byitem.go
///
/// ByItems wraps a "by" item.
#[derive(Clone, Debug)]
pub struct ByItems {
    /// Expr.
    pub expr: ExprRef,
    /// Desc.
    pub desc: bool,
}

/// go-file: pkg/planner/util/handle_cols.go (boundary stub)
///
/// blocked-by:D09.x (`HandleCols` interface: `IntHandleCols` / `CommonHandleCols`); the
/// pilot needs `IsInt` and `GetCol`.
#[derive(Clone, Debug)]
pub enum HandleCols {
    /// `*IntHandleCols`.
    Int(Arc<Column>),
    /// `*CommonHandleCols`.
    Common(Vec<Arc<Column>>),
}

impl HandleCols {
    /// IsInt returns if the HandleCols is a single int column.
    // glue: stub of HandleCols.IsInt
    pub fn is_int(&self) -> bool {
        matches!(self, HandleCols::Int(_))
    }

    /// GetCol gets the column by idx.
    // glue: stub of HandleCols.GetCol
    pub fn get_col(&self, idx: usize) -> Arc<Column> {
        match self {
            HandleCols::Int(c) => c.clone(),
            HandleCols::Common(cs) => cs[idx].clone(),
        }
    }
}

/// ResetNotNullFlag resets the not null flag of [start, end] columns in the schema.
///
/// Go copies the `*Column` (`col := *schema.Columns[i]`) and its `RetType` before
/// changing the flag: copy-on-write already, so the same in Rust.
// go: pkg/planner/util/null_misc.go ResetNotNullFlag L1 [M-EXPR]
pub fn reset_not_null_flag(schema: &mut Schema, start: usize, end: usize) {
    for i in start..end {
        let mut col = (*schema.columns[i]).clone();
        col.ret_type_mut().del_flag(NotNullFlag);
        schema.columns[i] = Arc::new(col);
    }
}
