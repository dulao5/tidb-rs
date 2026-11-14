//! go-file: pkg/sessiontxn/isolation/serializable.go

use super::base::{BaseTxnContextProvider, Kind};

/// PessimisticSerializableTxnContextProvider provides txn context for isolation level
/// oracle-like serializable (the [`Kind::PessimisticSerializable`] provider).
pub type PessimisticSerializableTxnContextProvider = BaseTxnContextProvider;

/// NewPessimisticSerializableTxnContextProvider returns a new
/// PessimisticSerializableTxnContextProvider
// go: NewPessimisticSerializableTxnContextProvider L1
pub fn new_pessimistic_serializable_txn_context_provider(
    causal_consistency_only: bool,
) -> PessimisticSerializableTxnContextProvider {
    BaseTxnContextProvider::with_kind(causal_consistency_only, Kind::PessimisticSerializable)
}

impl BaseTxnContextProvider {
    /// OnStmtErrorForNextAction is the hook that should be called when a new statement get
    /// an error
    // go: PessimisticSerializableTxnContextProvider.OnStmtErrorForNextAction L1 [M-ERRVAL]
    pub(crate) fn serializable_on_stmt_error_for_next_action(
        &mut self,
        point: crate::StmtErrorHandlePoint,
        err: tidb_base::Error,
    ) -> (crate::StmtErrorAction, Result<(), tidb_base::Error>) {
        match point {
            crate::StmtErrorHandlePoint::StmtErrAfterPessimisticLock => crate::error_action(err),
            _ => crate::no_idea(),
        }
    }
}
