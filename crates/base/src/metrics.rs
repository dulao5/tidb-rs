//! go-file: pkg/metrics/server.go
//!
//! The one function of `pkg/metrics` that sits in block D00.2 (it only reads
//! `terror.Error.RFCCode`); the metrics themselves are block D14.1.

use std::borrow::Cow;

use crate::errors::{self, Error};

/// ExecuteErrorToLabel converts an execute error to label.
// go: ExecuteErrorToLabel L1 [M-ERR]
pub fn execute_error_to_label(err: &Error) -> Cow<'_, str> {
    let err = errors::cause(err);
    if err.is_normalized() {
        return err.rfc_code();
    }
    Cow::Borrowed("unknown")
}
