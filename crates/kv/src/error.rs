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
    /// Any other client error.
    #[error("TiKV client error: {0}")]
    Other(#[source] Box<tikv_client::Error>),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<tikv_client::Error> for Error {
    fn from(e: tikv_client::Error) -> Error {
        use tikv_client::Error as E;
        let kind: fn(Box<E>) -> Error = match &e {
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
}
