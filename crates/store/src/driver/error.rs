//! go-file: pkg/store/driver/error/error.go
//!
//! The TiKV error instances and `ToTiDBErr`, which turns a client-go error into a TiDB
//! error.
//!
//! client-go is replaced by the client-rust fork (L3, ADR 0008), so the errors `ToTiDBErr`
//! receives are client-rust's [`tikv_client::Error`]s. [`client_go_error`] says which
//! client-go error a client-rust error stands for (the identity Go's `errors.Is` /
//! `errors.As` tests); `to_tidb_err` then keeps Go's order of checks. client-go errors
//! client-rust never returns (latches, the write path, TiFlash, the SQL killer's
//! interrupt signals, PD resource groups) are listed in that order as comments.

// Go identifiers for the error instances (M-GONAME).
#![allow(non_upper_case_globals)]

use std::sync::LazyLock;

use tidb_base::Error;
use tidb_base::dbterror::ClassTiKV;
use tidb_base::errno;
use tidb_base::gofmt::Arg;
use tidb_base::terror;
use tikv_client::metrics::BackoffType;
use tikv_client::proto::kvrpcpb;

/// ErrTokenLimit is the error that token is up to the limit.
pub static ErrTokenLimit: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiKVStoreLimit));
/// ErrTiKVServerTimeout is the error when tikv server is timeout.
pub static ErrTiKVServerTimeout: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiKVServerTimeout));
pub static ErrTiFlashServerTimeout: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiFlashServerTimeout));
/// ErrGCTooEarly is the error that GC life time is shorter than transaction duration
pub static ErrGCTooEarly: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrGCTooEarly));
/// ErrTiKVStaleCommand is the error that the command is stale in tikv.
pub static ErrTiKVStaleCommand: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiKVStaleCommand));
/// ErrQueryInterrupted is the error when the query is interrupted.
pub static ErrQueryInterrupted: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrQueryInterrupted));
/// ErrTiKVMaxTimestampNotSynced is the error that tikv's max timestamp is not synced.
pub static ErrTiKVMaxTimestampNotSynced: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiKVMaxTimestampNotSynced));
/// ErrLockAcquireFailAndNoWaitSet is the error that acquire the lock failed while no wait is
/// set.
pub static ErrLockAcquireFailAndNoWaitSet: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrLockAcquireFailAndNoWaitSet));
pub static ErrResolveLockTimeout: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrResolveLockTimeout));
/// ErrLockWaitTimeout is the error that wait for the lock is timeout.
pub static ErrLockWaitTimeout: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrLockWaitTimeout));
/// ErrTiKVServerBusy is the error when tikv server is busy.
pub static ErrTiKVServerBusy: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiKVServerBusy));
/// ErrTiFlashServerBusy is the error that tiflash server is busy.
pub static ErrTiFlashServerBusy: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrTiFlashServerBusy));
/// ErrPDServerTimeout is the error when pd server is timeout.
pub static ErrPDServerTimeout: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrPDServerTimeout));
/// ErrRegionUnavailable is the error when region is not available.
pub static ErrRegionUnavailable: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrRegionUnavailable));
/// ErrResourceGroupNotExists is the error when resource group does not exist.
pub static ErrResourceGroupNotExists: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrResourceGroupNotExists));
/// ErrResourceGroupConfigUnavailable is the error when resource group configuration is
/// unavailable.
pub static ErrResourceGroupConfigUnavailable: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrResourceGroupConfigUnavailable));
/// ErrResourceGroupThrottled is the error when resource group is exceeded quota limitation
pub static ErrResourceGroupThrottled: LazyLock<Error> =
    LazyLock::new(|| ClassTiKV.new_std(errno::ErrResourceGroupThrottled));
/// ErrUnknown is the unknow error.
pub static ErrUnknown: LazyLock<Error> = LazyLock::new(|| ClassTiKV.new_std(errno::ErrUnknown));

/// An error of the client-go surface the driver calls (`txnsnapshot.KVSnapshot`,
/// `tikv.KVStore`, `retry.Backoffer`), as the client-rust adapter returns it.
#[derive(Debug)]
pub enum TiKVError {
    /// `tikverr.ErrNotExist`: client-rust returns `Ok(None)` for a missing key, which the
    /// adapter turns into this.
    NotExist,
    /// An error of client-rust.
    Client(Box<tikv_client::Error>),
    /// A plain Go error the client-go code builds itself (`errors.Errorf`), already a TiDB
    /// error; `ToTiDBErr` returns it as is (`errors.Trace`).
    Plain(Error),
}

impl From<tikv_client::Error> for TiKVError {
    // glue: client-rust error into the boundary error
    fn from(e: tikv_client::Error) -> TiKVError {
        TiKVError::Client(Box::new(e))
    }
}

/// The client-go error a [`TiKVError`] stands for: the cases of `ToTiDBErr` and of
/// txn/error.go `extractKeyErr` that client-rust can produce.
#[derive(Debug)]
pub(crate) enum ClientGoError<'a> {
    /// `tikverr.ErrNotExist`.
    NotExist,
    /// `*tikverr.ErrWriteConflict` (from a `KeyError.conflict`).
    WriteConflict(&'a kvrpcpb::WriteConflict),
    /// `*tikverr.ErrRetryable` (from a `KeyError.retryable`).
    Retryable(&'a str),
    /// `tikverr.ErrTiKVServerTimeout` (a `tikvRPC` budget ran out).
    TiKVServerTimeout,
    /// `tikverr.ErrTiKVServerBusy` (a `tikvServerBusy` budget ran out).
    TiKVServerBusy,
    /// `tikverr.ErrTiKVStaleCommand`.
    TiKVStaleCommand,
    /// `tikverr.ErrResolveLockTimeout` (a `txnLockFast` / `txnNotFound` budget ran out, or
    /// client-rust gave up resolving a lock).
    ResolveLockTimeout,
    /// `tikverr.ErrRegionUnavailable` (a `regionMiss` / `regionScheduling` budget ran out,
    /// or client-rust returned a region error it did not retry).
    RegionUnavailable,
    /// `tikverr.ErrResultUndetermined`.
    Undetermined,
    /// Any other error: Go `errors.Trace(err)`, the error with its message.
    Other(Error),
}

/// The client-go error a client-rust error stands for (ADR 0008).
///
/// - A spent time budget ([`tikv_client::Error::BackoffExhausted`]) is client-go's
///   `longestSleepCfg.err` (config/retry/config.go:122-140): the error of the kind it slept on
///   longest.
/// - A per-key error is client-go's `tikverr.ExtractKeyErr` (error/error.go:304).
/// - Region errors client-rust returns without retrying them under the budget, and
///   `LeaderNotFound`, are what client-go reports after retrying them under `BoRegionMiss`:
///   `ErrRegionUnavailable`. A lock client-rust could not resolve is client-go's
///   `ErrResolveLockTimeout`. This is also how the PoC classifies them (`poc-kv` error.rs).
/// - Everything else (gRPC and PD errors client-rust did not retry, internal errors) keeps
///   its message: Go's `errors.Trace`.
// go-l3: ADR-0008 client-go error ExtractKeyErr / config/retry Config.err
pub(crate) fn client_go_error(err: &TiKVError) -> ClientGoError<'_> {
    use tikv_client::Error as E;
    let e = match err {
        TiKVError::NotExist => return ClientGoError::NotExist,
        TiKVError::Plain(e) => return ClientGoError::Other(e.clone()),
        TiKVError::Client(e) => &**e,
    };
    match e {
        E::BackoffExhausted { kind, .. } => match kind {
            BackoffType::TikvRpc => ClientGoError::TiKVServerTimeout,
            BackoffType::TikvLockFast | BackoffType::TxnNotFound => {
                ClientGoError::ResolveLockTimeout
            }
            BackoffType::RegionMiss | BackoffType::RegionScheduling => {
                ClientGoError::RegionUnavailable
            }
            BackoffType::ServerBusy => ClientGoError::TiKVServerBusy,
            BackoffType::StaleCommand => ClientGoError::TiKVStaleCommand,
        },
        E::UndeterminedError(_) => ClientGoError::Undetermined,
        E::KeyError(k) => extract_key_err(k),
        // client-go stops at the first key error of a batch.
        E::MultipleKeyErrors(v) => match v.first() {
            Some(E::KeyError(k)) => extract_key_err(k),
            _ => ClientGoError::Other(Error::plain(e.to_string())),
        },
        E::ResolveLockError(_) => ClientGoError::ResolveLockTimeout,
        E::RegionError(_)
        | E::MultipleRegionErrors(_)
        | E::NoCurrentRegions
        | E::EntryNotFoundInRegionCache
        | E::RegionForKeyNotFound { .. }
        | E::RegionForRangeNotFound { .. }
        | E::RegionNotFoundInResponse { .. }
        | E::LeaderNotFound { .. } => ClientGoError::RegionUnavailable,
        E::TxnNotFound(t) => ClientGoError::Other(tidb_base::errors::errorf(
            "txn %d not found",
            &[Arg::from(t.start_ts)],
        )),
        _ => ClientGoError::Other(Error::plain(e.to_string())),
    }
}

/// client-go `tikverr.ExtractKeyErr` (error/error.go:304): the error of a
/// `kvrpcpb.KeyError`. The assertion-failed case is the write path (not returned by reads).
// go-l3: ADR-0008 client-go error ExtractKeyErr
fn extract_key_err(key_err: &kvrpcpb::KeyError) -> ClientGoError<'_> {
    if let Some(conflict) = &key_err.conflict {
        return ClientGoError::WriteConflict(conflict);
    }
    if !key_err.retryable.is_empty() {
        return ClientGoError::Retryable(&key_err.retryable);
    }
    if !key_err.abort.is_empty() {
        let err =
            tidb_base::errors::errorf("tikv aborts txn: %s", &[Arg::from(key_err.abort.clone())]);
        log::warn!("2PC failed: {err}");
        return ClientGoError::Other(err);
    }
    if let Some(too_large) = &key_err.commit_ts_too_large {
        let err = tidb_base::errors::errorf(
            "commit TS %v is too large",
            &[Arg::from(too_large.commit_ts)],
        );
        log::warn!("2PC failed: {err}");
        return ClientGoError::Other(err);
    }
    if let Some(not_found) = &key_err.txn_not_found {
        return ClientGoError::Other(tidb_base::errors::errorf(
            "txn %d not found",
            &[Arg::from(not_found.start_ts)],
        ));
    }
    ClientGoError::Other(tidb_base::errors::errorf(
        "unexpected KeyError: %s",
        &[Arg::from(format!("{key_err:?}"))],
    ))
}

/// ToTiDBErr checks and converts a tikv error to a tidb error.
///
/// Go's checks in Go's order; the ones whose client-go error client-rust never returns are
/// comments.
// go: ToTiDBErr L3
pub fn to_tidb_err(err: TiKVError) -> Error {
    let e = client_go_error(&err);
    if matches!(e, ClientGoError::NotExist) {
        return tidb_kv::error::ErrNotExist.clone();
    }

    // *tikverr.ErrWriteConflictInLatch, *tikverr.ErrTxnTooLarge, tikverr.ErrCannotSetNilValue,
    // *tikverr.ErrEntryTooLarge, *tikverr.ErrKeyTooLarge, tikverr.ErrInvalidTxn: the write
    // path (client-rust's transactions are not used for writes by TiDB code).

    if matches!(e, ClientGoError::TiKVServerTimeout) {
        return ErrTiKVServerTimeout.clone();
    }

    // *tikverr.ErrPDServerTimeout: client-rust's PD errors keep their message.
    // tikverr.ErrTiFlashServerTimeout: TiFlash is DEF.MPP.
    // tikverr.ErrQueryInterruptedWithSignal{...}: client-rust has no SQL killer hook (KILL
    // stops a read by dropping its future, N3).

    if matches!(e, ClientGoError::TiKVServerBusy) {
        return ErrTiKVServerBusy.clone();
    }

    // tikverr.ErrTiFlashServerBusy: TiFlash.
    // *tikverr.ErrGCTooEarly: client-rust has no GC safe-point check (client-go-parity.md §4,
    // CheckVisibility).

    if matches!(e, ClientGoError::TiKVStaleCommand) {
        return ErrTiKVStaleCommand.clone();
    }

    // tikverr.ErrTiKVMaxTimestampNotSynced, tikverr.ErrLockAcquireFailAndNoWaitSet: the write
    // path.

    if matches!(e, ClientGoError::ResolveLockTimeout) {
        return ErrResolveLockTimeout.clone();
    }

    // tikverr.ErrLockWaitTimeout: pessimistic locking (write path).

    if matches!(e, ClientGoError::RegionUnavailable) {
        return ErrRegionUnavailable.clone();
    }

    // *tikverr.ErrTokenLimit: client-rust has no store token limit.
    // tikverr.ErrUnknown: not returned by client-rust.

    if matches!(e, ClientGoError::Undetermined) {
        return terror::ErrResultUndetermined.clone();
    }

    // *pderr.ErrClientGetResourceGroup, pderr.ErrClientResourceGroupConfigUnavailable,
    // pderr.ErrClientResourceGroupThrottled: resource control is N10 (not in client-rust).

    match e {
        ClientGoError::Other(err) => err,
        // The write conflict and retryable errors are extractKeyErr's; ToTiDBErr traces them.
        ClientGoError::WriteConflict(_) | ClientGoError::Retryable(_) => match err {
            TiKVError::Client(c) => Error::plain(c.to_string()),
            TiKVError::Plain(p) => p,
            TiKVError::NotExist => unreachable!(),
        },
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exhausted(kind: BackoffType) -> TiKVError {
        tikv_client::Error::BackoffExhausted {
            kind,
            last: Box::new(tikv_client::Error::StringError("last".to_owned())),
        }
        .into()
    }

    /// client-go's `longestSleepCfg` errors, through `ToTiDBErr`.
    #[test]
    fn spent_budgets() {
        for (kind, code) in [
            (BackoffType::TikvRpc, errno::ErrTiKVServerTimeout),
            (BackoffType::TikvLockFast, errno::ErrResolveLockTimeout),
            (BackoffType::TxnNotFound, errno::ErrResolveLockTimeout),
            (BackoffType::RegionMiss, errno::ErrRegionUnavailable),
            (BackoffType::RegionScheduling, errno::ErrRegionUnavailable),
            (BackoffType::ServerBusy, errno::ErrTiKVServerBusy),
            (BackoffType::StaleCommand, errno::ErrTiKVStaleCommand),
        ] {
            let e = to_tidb_err(exhausted(kind));
            assert_eq!(e.code(), code as isize, "{kind:?}");
        }
        assert_eq!(
            to_tidb_err(exhausted(BackoffType::RegionMiss)).to_string(),
            "[tikv:9005]Region is unavailable"
        );
    }

    #[test]
    fn not_exist_and_region_errors() {
        assert!(tidb_kv::error::is_err_not_found(&to_tidb_err(
            TiKVError::NotExist
        )));
        let leader = tikv_client::Error::LeaderNotFound {
            region: tikv_client::RegionVerId {
                id: 1,
                conf_ver: 1,
                ver: 1,
            },
        };
        assert!(ErrRegionUnavailable.equal(&to_tidb_err(leader.into())));
        assert!(ErrResolveLockTimeout.equal(&to_tidb_err(
            tikv_client::Error::ResolveLockError(vec![]).into()
        )));
        assert!(
            terror::ErrResultUndetermined.equal(&to_tidb_err(
                tikv_client::Error::UndeterminedError(Box::new(tikv_client::Error::StringError(
                    "x".into()
                )))
                .into()
            ))
        );
    }

    #[test]
    fn key_errors() {
        let abort = kvrpcpb::KeyError {
            abort: "boom".into(),
            ..Default::default()
        };
        let e = to_tidb_err(tikv_client::Error::KeyError(Box::new(abort)).into());
        assert_eq!(e.to_string(), "tikv aborts txn: boom");
        let nf = kvrpcpb::KeyError {
            txn_not_found: Some(kvrpcpb::TxnNotFound {
                start_ts: 7,
                ..Default::default()
            }),
            ..Default::default()
        };
        let e = to_tidb_err(
            tikv_client::Error::MultipleKeyErrors(vec![tikv_client::Error::KeyError(Box::new(nf))])
                .into(),
        );
        assert_eq!(e.to_string(), "txn 7 not found");
        let other = to_tidb_err(tikv_client::Error::StringError("plain".into()).into());
        assert_eq!(other.to_string(), "plain");
    }
}
