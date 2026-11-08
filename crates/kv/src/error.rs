//! go-file: pkg/kv/error.go
//!
//! The KV errors (M-ERR, M-GONAME). `GenKeyExistsErr` is the write path (X).

#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassKV;
use tidb_base::dbterror::ClassTiKV;
use tidb_base::errno;
use tidb_base::mysql;

/// TxnRetryableMark is used to uniform the commit error messages which could retry the
/// transaction.
/// *WARNING*: changing this string will affect the backward compatibility.
pub const TxnRetryableMark: &str = "[try again later]";

/// `pmysql.Message(mysql.MySQLErrName[code].Raw + suffix, RedactArgPos)`.
// glue: message built at package initialisation (Go concatenates at init)
fn message_with_suffix(code: u16, suffix: &str) -> mysql::ErrMessage {
    let base = errno::MySQLErrName
        .get(&code)
        .expect("registered error message");
    let raw: &'static str = Box::leak(format!("{}{}", base.raw, suffix).into_boxed_str());
    mysql::message(raw, base.redact_arg_pos)
}

/// ErrNotExist is used when try to get an entry with an unexist key from KV store.
pub static ErrNotExist: LazyLock<Error> = LazyLock::new(|| ClassKV.new_std(errno::ErrNotExist));
/// ErrTxnRetryable is used when KV store occurs retryable error which SQL layer can safely
/// retry the transaction.
/// When using TiKV as the storage node, the error is returned ONLY when lock not found
/// (txnLockNotFound) in Commit, subject to change it in the future.
pub static ErrTxnRetryable: LazyLock<Error> = LazyLock::new(|| {
    ClassKV.new_std_err(
        errno::ErrTxnRetryable,
        &message_with_suffix(errno::ErrTxnRetryable, TxnRetryableMark),
    )
});
/// ErrCannotSetNilValue is the error when sets an empty value.
pub static ErrCannotSetNilValue: LazyLock<Error> =
    LazyLock::new(|| ClassKV.new_std(errno::ErrCannotSetNilValue));
/// ErrInvalidTxn is the error when commits or rollbacks in an invalid transaction.
pub static ErrInvalidTxn: LazyLock<Error> = LazyLock::new(|| ClassKV.new_std(errno::ErrInvalidTxn));
/// ErrTxnTooLarge is the error when transaction is too large, lock time reached the
/// maximum value.
pub static ErrTxnTooLarge: LazyLock<Error> =
    LazyLock::new(|| ClassKV.new_std(errno::ErrTxnTooLarge));
/// ErrEntryTooLarge is the error when a key value entry is too large.
pub static ErrEntryTooLarge: LazyLock<Error> =
    LazyLock::new(|| ClassKV.new_std(errno::ErrEntryTooLarge));
/// ErrKeyTooLarge is the error when a key is too large to be handled by MemBuffer.
pub static ErrKeyTooLarge: LazyLock<Error> =
    LazyLock::new(|| ClassKV.new_std(errno::ErrKeyTooLarge));
/// ErrKeyExists returns when key is already exist. Caller should try to use
/// GenKeyExistsErr to generate this error for correct format.
pub static ErrKeyExists: LazyLock<Error> = LazyLock::new(|| ClassKV.new_std(errno::ErrDupEntry));
/// ErrNotImplemented returns when a function is not implemented yet.
pub static ErrNotImplemented: LazyLock<Error> =
    LazyLock::new(|| ClassKV.new_std(errno::ErrNotImplemented));
/// ErrWriteConflict is the error when the commit meets an write conflict error.
pub static ErrWriteConflict: LazyLock<Error> = LazyLock::new(|| {
    ClassKV.new_std_err(
        errno::ErrWriteConflict,
        &message_with_suffix(errno::ErrWriteConflict, &format!(" {TxnRetryableMark}")),
    )
});
/// ErrWriteConflictInTiDB is the error when the commit meets an write conflict error when
/// local latch is enabled.
pub static ErrWriteConflictInTiDB: LazyLock<Error> = LazyLock::new(|| {
    ClassKV.new_std_err(
        errno::ErrWriteConflictInTiDB,
        &message_with_suffix(
            errno::ErrWriteConflictInTiDB,
            &format!(" {TxnRetryableMark}"),
        ),
    )
});
/// ErrLockExpire is the error when the lock is expired.
pub static ErrLockExpire: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrLockExpire));
/// ErrAssertionFailed is the error when an assertion fails.
pub static ErrAssertionFailed: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrAssertionFailed));

/// IsTxnRetryableError checks if the error could safely retry the transaction.
// go: IsTxnRetryableError L1
pub fn is_txn_retryable_error(err: Option<&Error>) -> bool {
    let Some(err) = err else {
        return false;
    };
    if ErrTxnRetryable.equal(err)
        || ErrWriteConflict.equal(err)
        || ErrWriteConflictInTiDB.equal(err)
    {
        return true;
    }
    false
}

/// IsErrNotFound checks if err is a kind of NotFound error.
// go: IsErrNotFound L1
pub fn is_err_not_found(err: &Error) -> bool {
    ErrNotExist.equal(err)
}
