/// Errors of KV reads, grouped by what a caller can do about them. The client error is
/// boxed to keep `Result`s small.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A lock could not be resolved within the lock retry budget (TiDB:
    /// `ErrResolveLockTimeout`). The statement can be retried with a newer timestamp.
    #[error("resolve lock timeout: {0}")]
    LockTimeout(#[source] Box<tikv_client::Error>),
    /// Region errors persisted beyond the region retry budget (TiDB: `ErrRegionUnavailable`).
    #[error("region unavailable: {0}")]
    RegionUnavailable(#[source] Box<tikv_client::Error>),
    /// TiKV rejected the read for one or more keys (e.g. the snapshot is older than the GC
    /// safe point, or the coprocessor reported an error).
    #[error("TiKV key error: {0}")]
    KeyError(#[source] Box<tikv_client::Error>),
    /// PD or TiKV could not be reached, or a gRPC call failed.
    #[error("TiKV/PD unavailable: {0}")]
    Unavailable(#[source] Box<tikv_client::Error>),
    /// An error of the coprocessor client (`tidb_store::copr`, D07.2): already a TiDB error.
    #[error("{0}")]
    Copr(tidb_base::errors::Error),
    /// A GC safe point visibility check failed (`9006` `ErrGCTooEarly` or `9001`
    /// `ErrPDServerTimeout`).
    #[error("{0}")]
    SafePoint(Box<tidb_base::Error>),
    /// Any other client error.
    #[error("TiKV client error: {0}")]
    Other(#[source] Box<tikv_client::Error>),
    /// An error of the ported TiKV store driver, already a TiDB error
    /// (`TIDB_RS_POINT_GET=store-driver`).
    #[error("{0}")]
    Driver(Box<tidb_base::Error>),
}

impl Error {
    /// Returns the underlying normalized TiDB error when this KV error already carries one
    /// (`SafePoint`, `Copr`, or `Driver`), so SQL error mapping preserves its MySQL error code.
    pub fn as_tidb_error(&self) -> Option<&tidb_base::Error> {
        match self {
            Error::SafePoint(e) | Error::Driver(e) => Some(e),
            Error::Copr(e) => Some(e),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<tikv_client::Error> for Error {
    fn from(e: tikv_client::Error) -> Error {
        use tikv_client::Error as E;
        let kind: fn(Box<E>) -> Error = match &e {
            // A read's retry budget ran out (client-rust#13): classified by the kind of error it
            // slept on longest, as TiDB maps client-go's `longestSleepCfg` error (tikvRPC →
            // 9002 ErrTiKVServerTimeout, regionMiss / regionScheduling → 9005
            // ErrRegionUnavailable, staleCommand → 9010, txnLockFast / txnNotFound → 9004
            // ErrResolveLockTimeout).
            E::BackoffExhausted { kind, .. } => {
                use tikv_client::metrics::BackoffType as B;
                match kind {
                    B::RegionMiss | B::RegionScheduling | B::StaleCommand => {
                        Error::RegionUnavailable
                    }
                    B::TikvLockFast | B::TxnNotFound => Error::LockTimeout,
                    B::TikvRpc | B::ServerBusy => Error::Unavailable,
                }
            }
            E::ResolveLockError(_) => Error::LockTimeout,
            E::RegionError(_)
            | E::MultipleRegionErrors(_)
            | E::NoCurrentRegions
            | E::EntryNotFoundInRegionCache
            | E::RegionForKeyNotFound { .. }
            | E::RegionForRangeNotFound { .. }
            | E::RegionNotFoundInResponse { .. }
            | E::LeaderNotFound { .. } => Error::RegionUnavailable,
            E::KeyError(_) | E::MultipleKeyErrors(_) => Error::KeyError,
            E::Grpc(_) | E::GrpcAPI(_) | E::Url(_) | E::Io(_) => Error::Unavailable,
            _ => Error::Other,
        };
        kind(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_budgets_are_classified_by_their_longest_sleep() {
        use tikv_client::metrics::BackoffType as B;
        let exhausted = |kind| tikv_client::Error::BackoffExhausted {
            kind,
            last: Box::new(tikv_client::Error::StringError("last".to_owned())),
        };
        for kind in [B::RegionMiss, B::RegionScheduling, B::StaleCommand] {
            assert!(matches!(
                Error::from(exhausted(kind)),
                Error::RegionUnavailable(_)
            ));
        }
        for kind in [B::TikvLockFast, B::TxnNotFound] {
            assert!(matches!(
                Error::from(exhausted(kind)),
                Error::LockTimeout(_)
            ));
        }
        assert!(matches!(
            Error::from(exhausted(B::TikvRpc)),
            Error::Unavailable(_)
        ));
    }

    #[test]
    fn client_errors_are_classified() {
        assert!(matches!(
            Error::from(tikv_client::Error::ResolveLockError(vec![])),
            Error::LockTimeout(_)
        ));
        assert!(matches!(
            Error::from(tikv_client::Error::NoCurrentRegions),
            Error::RegionUnavailable(_)
        ));
        assert!(matches!(
            Error::from(tikv_client::Error::MultipleKeyErrors(vec![])),
            Error::KeyError(_)
        ));
        assert!(matches!(
            Error::from(tikv_client::Error::StringError("x".to_owned())),
            Error::Other(_)
        ));
    }

    #[test]
    fn safepoint_and_driver_errors_expose_normalized_tidb_error() {
        let cache = crate::SafePointCache::new();
        let sp = (1_700_000_060_000_u64 << 18) + 1;
        cache.update(sp, std::time::Instant::now());
        let tidb_err = cache.check_visibility(sp - 10).unwrap_err();
        let kv_err = Error::SafePoint(Box::new(tidb_err.clone()));
        assert_eq!(kv_err.as_tidb_error().unwrap().code(), 9006);

        let drv_err = Error::Driver(Box::new(tidb_err));
        assert_eq!(drv_err.as_tidb_error().unwrap().code(), 9006);

        let stale = crate::SafePointCache::new();
        stale.update(
            10,
            std::time::Instant::now() - std::time::Duration::from_secs(100),
        );
        let timeout_err = Error::SafePoint(Box::new(stale.check_visibility(20).unwrap_err()));
        assert_eq!(timeout_err.as_tidb_error().unwrap().code(), 9001);
    }
}
