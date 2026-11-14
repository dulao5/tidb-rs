//! go-file: pkg/sessiontxn/interface.go
//! go-file: pkg/sessiontxn/future.go
//!
//! Block D05.4 part 1 (specs/D05.4-sessiontxn): the transaction context providers of the
//! read path. N0 rank 11.
//!
//! - [`isolation`]: `pkg/sessiontxn/isolation`, the optimistic, pessimistic repeatable-read,
//!   read-committed and serializable providers (their read path: start ts, the TSO
//!   prefetch, the per-statement ts of RC, the autocommit point-get MaxTS);
//! - [`staleread`]: the provider skeleton of stale reads (part 2) and `IsStmtStaleness`;
//! - [`internal`]: the package `pkg/sessiontxn/internal`;
//! - [`sctx`]: glue, the session / store / planner calls the package makes (traits).
//!
//! The write path (pessimistic locks and for-update ts, fair locking, the commit options,
//! optimistic retry, assertion levels, temporary-table KV filters) is not ported: tidb-rs
//! is read-only, and writes in explicit transactions are rejected by the session (N11 D8,
//! 1792). D05.4 spec §1.

pub mod internal;
pub mod isolation;
pub mod sctx;
pub mod staleread;

use futures::future::BoxFuture;
use tidb_base::Error;

use sctx::{InfoSchemaRef, PlanInfo, SessionCtx, StmtInfo};

/// EnterNewTxnType is the type to enter a new txn
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnterNewTxnType {
    /// EnterNewTxnDefault means to enter a new txn. Its behavior is more straight-forward
    /// just starting a new txn right now without any scenario assumptions.
    EnterNewTxnDefault,
    /// EnterNewTxnWithBeginStmt indicates to enter a new txn when execute 'BEGIN' or 'START TRANSACTION'
    EnterNewTxnWithBeginStmt,
    /// EnterNewTxnBeforeStmt indicates to enter a new txn before each statement when the txn
    /// is not present. If `EnterNewTxnBeforeStmt` is used, the new txn will always act as the
    /// 'lazy' mode. That means the inner transaction is only activated when needed to reduce
    /// unnecessary overhead.
    EnterNewTxnBeforeStmt,
    /// EnterNewTxnWithReplaceProvider indicates to replace the current provider. Now only
    /// stale read are using this
    EnterNewTxnWithReplaceProvider,
}

/// EnterNewTxnRequest is the request when entering a new transaction
pub struct EnterNewTxnRequest {
    /// Type is the type for new entering a new txn
    pub tp: EnterNewTxnType,
    /// provider is the context provider
    pub provider: Option<Box<dyn TxnContextProvider>>,
    /// txnMode is the transaction mode for the new txn. It has 3 values: `ast.Pessimistic`
    /// ,`ast.Optimistic` or empty. When the value is empty, it means the value will be
    /// determined from sys vars.
    pub txn_mode: String,
    /// causalConsistencyOnly means whether enable causal consistency for transactions,
    /// default is false
    pub causal_consistency_only: bool,
    /// staleReadTS indicates the read ts for the stale read transaction. The default value
    /// is zero which means not a stale read transaction.
    pub stale_read_ts: u64,
}

/// StmtErrorHandlePoint is where the error is being handled
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmtErrorHandlePoint {
    /// StmtErrAfterQuery means we are handling an error after the query failed
    StmtErrAfterQuery,
    /// StmtErrAfterPessimisticLock means we are handling an error after pessimistic lock failed.
    StmtErrAfterPessimisticLock,
}

/// StmtErrorAction is the next action advice when an error occurs when executing a statement
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StmtErrorAction {
    /// StmtActionError means the error should be returned directly without any retry
    StmtActionError,
    /// StmtActionRetryReady means the error is caused by this component, and it is ready for retry.
    StmtActionRetryReady,
    /// StmtActionNoIdea means the error is not caused by this component, and whether retry
    /// or not should be determined by other components. If the user do not know whether to
    /// retry or not, it is advised to return the original error.
    StmtActionNoIdea,
}

/// ErrorAction returns StmtActionError with specified error
// go: ErrorAction L1 [M-ERRVAL]
pub fn error_action(err: Error) -> (StmtErrorAction, Result<(), Error>) {
    (StmtErrorAction::StmtActionError, Err(err))
}

/// RetryReady returns StmtActionRetryReady, nil
// go: RetryReady L1 [M-ERRVAL]
pub fn retry_ready() -> (StmtErrorAction, Result<(), Error>) {
    (StmtErrorAction::StmtActionRetryReady, Ok(()))
}

/// NoIdea returns StmtActionNoIdea, nil
// go: NoIdea L1 [M-ERRVAL]
pub fn no_idea() -> (StmtErrorAction, Result<(), Error>) {
    (StmtErrorAction::StmtActionNoIdea, Ok(()))
}

/// ConstantFuture implements oracle.Future
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConstantFuture(pub u64);

impl ConstantFuture {
    /// Wait returns a constant ts
    // go: ConstantFuture.Wait L1
    pub fn wait(self) -> Result<u64, Error> {
        Ok(self.0)
    }
}

/// TxnContextProvider provides txn context (Go also embeds `TxnAdvisable`).
///
/// The read half of Go's interface; `GetStmtForUpdateTS`, `GetSnapshotWithStmtForUpdateTS`,
/// `OnPessimisticStmtStart/End`, `SetOptionsBeforeCommit` are the write path. The session is
/// passed to each method (module [`sctx`]); the methods that wait for a TSO are async.
pub trait TxnContextProvider: Send {
    /// AdviseWarmup provides warmup for inner state
    // glue: interface method TxnAdvisable.AdviseWarmup
    fn advise_warmup(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error>;
    /// AdviseOptimizeWithPlan providers optimization according to the plan (`plan any`:
    /// `None` is a value that is not a `base.Plan`).
    // glue: interface method TxnAdvisable.AdviseOptimizeWithPlan
    fn advise_optimize_with_plan(
        &mut self,
        sctx: &mut dyn SessionCtx,
        plan: Option<&dyn PlanInfo>,
    ) -> Result<(), Error>;
    /// GetTxnInfoSchema returns the information schema used by txn
    // glue: interface method TxnContextProvider.GetTxnInfoSchema
    fn get_txn_info_schema(&mut self, sctx: &mut dyn SessionCtx) -> Option<InfoSchemaRef>;
    /// GetTxnScope returns the current txn scope
    // glue: interface method TxnContextProvider.GetTxnScope
    fn get_txn_scope(&self, sctx: &dyn SessionCtx) -> String;
    /// GetReadReplicaScope returns the read replica scope
    // glue: interface method TxnContextProvider.GetReadReplicaScope
    fn get_read_replica_scope(&self, sctx: &dyn SessionCtx) -> String;
    /// GetStmtReadTS returns the read timestamp used by select statement (not for select ... for update)
    // glue: interface method TxnContextProvider.GetStmtReadTS
    fn get_stmt_read_ts<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<u64, Error>>;
    /// GetSnapshotWithStmtReadTS gets snapshot with read ts
    // glue: interface method TxnContextProvider.GetSnapshotWithStmtReadTS
    fn get_snapshot_with_stmt_read_ts<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<std::sync::Arc<dyn tidb_kv::kv::Snapshot>, Error>>;
    /// OnInitialize is the hook that should be called when enter a new txn with this provider
    // glue: interface method TxnContextProvider.OnInitialize
    fn on_initialize<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
        tp: EnterNewTxnType,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// OnStmtStart is the hook that should be called when a new statement started
    // glue: interface method TxnContextProvider.OnStmtStart
    fn on_stmt_start(
        &mut self,
        sctx: &mut dyn SessionCtx,
        node: Option<&dyn StmtInfo>,
    ) -> Result<(), Error>;
    /// OnStmtErrorForNextAction is the hook that should be called when a new statement get an error
    // glue: interface method TxnContextProvider.OnStmtErrorForNextAction
    fn on_stmt_error_for_next_action(
        &mut self,
        sctx: &mut dyn SessionCtx,
        point: StmtErrorHandlePoint,
        err: Error,
    ) -> (StmtErrorAction, Result<(), Error>);
    /// OnStmtRetry is the hook that should be called when a statement is retried internally.
    // glue: interface method TxnContextProvider.OnStmtRetry
    fn on_stmt_retry(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error>;
    /// OnStmtCommit is the hook that should be called when a statement is executed successfully.
    // glue: interface method TxnContextProvider.OnStmtCommit
    fn on_stmt_commit(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error>;
    /// OnStmtRollback is the hook that should be called when a statement fails to execute.
    // glue: interface method TxnContextProvider.OnStmtRollback
    fn on_stmt_rollback(
        &mut self,
        sctx: &mut dyn SessionCtx,
        is_for_pessimistic_retry: bool,
    ) -> Result<(), Error>;
    /// OnLocalTemporaryTableCreated is the hook that should be called when a local temporary table created.
    // glue: interface method TxnContextProvider.OnLocalTemporaryTableCreated
    fn on_local_temporary_table_created(&mut self, sctx: &mut dyn SessionCtx) -> Result<(), Error>;
    /// ActivateTxn activates the transaction.
    // glue: interface method TxnContextProvider.ActivateTxn
    fn activate_txn<'a>(
        &'a mut self,
        sctx: &'a mut dyn SessionCtx,
    ) -> BoxFuture<'a, Result<(), Error>>;
    /// Where the statement's read ts came from (N11 §4.4), after it got its read ts.
    // glue: N11 §4.4 ReadTsSource (no Go counterpart)
    fn read_ts_source(&self, sctx: &dyn SessionCtx) -> sctx::ReadTsSource;
    /// The TSO requests (regular or low-resolution) the current statement issued, counted
    /// from `OnStmtStart` (N11 §4.4: a `MaxTs` / `TxnStart` statement issues none).
    // glue: N11 §4.4 (no Go counterpart)
    fn stmt_tso_requests(&self) -> u32;
}

/// TxnManager is an interface providing txn context management in session (implemented by
/// the session's `txnManager`, D05.3): the methods the package's helpers call.
pub trait TxnManager: Send {
    // glue: interface method TxnManager.AdviseWarmup
    fn advise_warmup(&mut self) -> Result<(), Error>;
    // glue: interface method TxnManager.AdviseOptimizeWithPlan
    fn advise_optimize_with_plan(&mut self, plan: Option<&dyn PlanInfo>) -> Result<(), Error>;
    // glue: interface method TxnManager.EnterNewTxn
    fn enter_new_txn(&mut self, req: EnterNewTxnRequest) -> BoxFuture<'_, Result<(), Error>>;
    /// `OnStmtStart(ctx, GetCurrentStmt())`.
    // glue: interface methods TxnManager.OnStmtStart and TxnManager.GetCurrentStmt
    fn on_stmt_start_with_current_stmt(&mut self) -> Result<(), Error>;
}

/// AdviseOptimizeWithPlanAndThenWarmUp first do `AdviseOptimizeWithPlan` to optimize the txn
/// with plan and then do `AdviseWarmup` to do some tso fetch if necessary. Go reaches the
/// manager with `GetTxnManager(sctx)`; here the session's manager is passed.
// go: AdviseOptimizeWithPlanAndThenWarmUp L2 (the txn manager is passed, GetTxnManager is the session's)
pub fn advise_optimize_with_plan_and_then_warm_up(
    txn_manager: &mut dyn TxnManager,
    plan: Option<&dyn PlanInfo>,
) -> Result<(), Error> {
    txn_manager.advise_optimize_with_plan(plan)?;
    txn_manager.advise_warmup()
}

/// NewTxn starts a new optimistic and active txn, it can be used for the below scenes:
/// 1. Commit the current transaction and do some work in a new transaction for some
///    specific operations, for example: DDL
/// 2. Some background job need to do something in a transaction.
///
/// In other scenes like 'BEGIN', 'START TRANSACTION' or prepare transaction in a new
/// statement, you should use `TxnManager`.`EnterNewTxn` and pass the relevant to it.
// go: NewTxn L2 (the txn manager is passed, GetTxnManager is the session's)
pub async fn new_txn(txn_manager: &mut dyn TxnManager) -> Result<(), Error> {
    txn_manager
        .enter_new_txn(EnterNewTxnRequest {
            tp: EnterNewTxnType::EnterNewTxnDefault,
            provider: None,
            txn_mode: "OPTIMISTIC".to_owned(), // ast.Optimistic
            causal_consistency_only: false,
            stale_read_ts: 0,
        })
        .await
}

/// NewTxnInStmt is like `NewTxn` but it will call `OnStmtStart` after it. It should be used
/// when a statement already started.
// go: NewTxnInStmt L2 (the txn manager is passed, GetTxnManager is the session's)
pub async fn new_txn_in_stmt(txn_manager: &mut dyn TxnManager) -> Result<(), Error> {
    new_txn(txn_manager).await?;
    txn_manager.on_stmt_start_with_current_stmt()
}
