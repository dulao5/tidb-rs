//! go-file: pkg/expression/explain.go
//!
//! The leaf functions of the file needed by the planner utilities (VS1-19): the EXPLAIN text
//! of a column. They call only ported code (`Column.StringWithCtx`), so they are ported
//! ahead of the rest of the file (constitution §4.5); the other 17 functions stay with
//! D10.9 (`specs/D10.9-postprocess/tasks.md` P1-1).

use tidb_base::errors::RedactLogDisable;

use crate::column::Column;
use crate::context::{EvalContext, ParamValues};

impl Column {
    /// ColumnExplainInfo returns the explained info for column.
    // go: Column.ColumnExplainInfo L1
    pub fn column_explain_info(&self, ctx: &dyn ParamValues, normalized: bool) -> Vec<u8> {
        if normalized {
            return self.column_explain_info_normalized();
        }
        self.string_with_ctx(ctx, RedactLogDisable)
    }

    /// ColumnExplainInfoNormalized returns the normalized explained info for column.
    // go: Column.ColumnExplainInfoNormalized L1
    pub fn column_explain_info_normalized(&self) -> Vec<u8> {
        if !self.orig_name.is_empty() {
            return self.orig_name.as_bytes().to_vec();
        }
        b"?".to_vec()
    }

    /// ExplainInfo implements the Expression interface.
    // go: Column.ExplainInfo L1
    pub fn explain_info(&self, ctx: &dyn EvalContext) -> Vec<u8> {
        self.column_explain_info(ctx, false)
    }
}
