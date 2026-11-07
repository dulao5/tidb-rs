//! go-file: pkg/util/context/warn.go
//!
//! `WarnAppender`, the warning sink of `types.Context` (D01.1) and `errctx` (D00.1), moved
//! down to tidb-base (it only needs [`Error`]). The rest of `pkg/util/context` is block D00.1.

use crate::errors::Error;

/// WarnAppender provides a function to add a warning. Using interface rather than a simple
/// function/closure can avoid memory allocation in some cases. See
/// https://github.com/pingcap/tidb/issues/49277
pub trait WarnAppender {
    /// AppendWarning appends a warning
    // glue: interface method of contextutil.WarnAppender
    fn append_warning(&self, err: Error);
    /// AppendNote appends a warning with level 'Note'.
    // glue: interface method of contextutil.WarnAppender
    fn append_note(&self, msg: Error);
}
