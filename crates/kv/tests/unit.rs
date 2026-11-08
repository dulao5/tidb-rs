//! The assertions of the pkg/kv Go tests that the govec records do not express.

use tidb_base::Error;
use tidb_base::mysql;
use tidb_base::terror::to_sql_error;
use tidb_kv::error::*;

/// Go `TestError`: every KV error converts to its own SQL error code.
#[test]
fn error_codes() {
    let kv_errs: [&Error; 9] = [
        &ErrNotExist,
        &ErrTxnRetryable,
        &ErrCannotSetNilValue,
        &ErrInvalidTxn,
        &ErrTxnTooLarge,
        &ErrEntryTooLarge,
        &ErrNotImplemented,
        &ErrWriteConflict,
        &ErrWriteConflictInTiDB,
    ];
    for err in kv_errs {
        let code = to_sql_error(err).code;
        assert_ne!(code, mysql::ErrUnknown);
        assert_eq!(err.code() as u16, code);
    }
}
