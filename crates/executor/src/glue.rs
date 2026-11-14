//! glue: what the executor framework (D12.0, VS1-13) needs from blocks above it or not ported
//! yet, as narrow interfaces and stand-in types (constitution §2.4).
//!
//! - [`SessionContext`]: the `sessionctx.Context` methods (and the `sessiontxn` transaction
//!   manager calls) that `ExecStmt`, `recordSet`, the compiler and the executor builder make.
//!   The session (D05.3, rank 15, above this crate) implements it in VS1-14, as it implements
//!   `tidb_sessiontxn::sctx::SessionCtx` (#219).
//! - [`ResultField`]: `resolve.ResultField` (`pkg/planner/core/resolve/result.go`, D09.0,
//!   VS1-10); moves to the planner when D09.0 lands.
//! - [`PlanCacheStmt`]: the part of `plannercore.PlanCacheStmt` (D13.1, VS1-15) that
//!   `ExecStmt.PointGet` reads, the cached point get executor.
//! - [`Plan`]: Go's `base.Plan`. The new stack only builds physical plans in VS1 (no
//!   `Execute`, `Set`, `Simple`, ... plans), so it is the physical plan.

use std::sync::Arc;
use std::time::SystemTime;

use tidb_base::Error;
use tidb_memory::sqlkiller::SqlKiller;
use tidb_parser::model::CIStr;
use tidb_parser::types::FieldType;
use tidb_sessionctx::variable::session::SessionVars;

use crate::internal::exec::executor::Executor;

/// `infoschema.InfoSchema` (the new stack only knows v2, VS1 plan §4.1).
pub type InfoSchemaRef = Arc<tidb_infoschema::InfoSchemaV2>;

/// Go's `base.Plan` (see the module comment).
pub type Plan = tidb_planner::PhysicalPlanRef;

/// The `sessionctx.Context` surface of the executor framework (see the module comment).
pub trait SessionContext: Send {
    /// `GetSessionVars()`.
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars(&self) -> &SessionVars;
    /// `GetSessionVars()`, for writing.
    // glue: interface method sessionctx.Context.GetSessionVars
    fn get_session_vars_mut(&mut self) -> &mut SessionVars;
    /// `GetSessionVars().SQLKiller` (the session's shared part, N2 `SessionShared`).
    // glue: SessionVars.SQLKiller (N2 SessionShared)
    fn sql_killer(&self) -> Arc<SqlKiller>;
    /// `sessiontxn.GetTxnManager(sctx).GetStmtReadTS()`.
    // glue: interface method sessiontxn.TxnManager.GetStmtReadTS (D05.3 / D05.4, VS1-14)
    fn get_stmt_read_ts(&mut self) -> Result<u64, Error>;
    /// `sessiontxn.GetTxnManager(sctx).GetTxnInfoSchema()`.
    // glue: interface method sessiontxn.TxnManager.GetTxnInfoSchema (VS1-14)
    fn get_txn_info_schema(&self) -> Option<InfoSchemaRef>;
    /// `sessiontxn.GetTxnManager(sctx).GetTxnScope()`.
    // glue: interface method sessiontxn.TxnManager.GetTxnScope (VS1-14)
    fn get_txn_scope(&self) -> String;
    /// `sessiontxn.GetTxnManager(sctx).GetReadReplicaScope()`.
    // glue: interface method sessiontxn.TxnManager.GetReadReplicaScope (VS1-14)
    fn get_read_replica_scope(&self) -> String;
    /// `staleread.IsStmtStaleness(sctx)`.
    // glue: sessiontxn/staleread.IsStmtStaleness (D05.4)
    fn is_stmt_staleness(&self) -> bool;
    /// `sctx.Txn(false)`: the start ts of the transaction when it is valid, `None` otherwise.
    // glue: interface method sessionctx.Context.Txn (VS1-14)
    fn txn_start_ts(&mut self) -> Result<Option<u64>, Error>;
    /// `processinfoSetter.SetProcessInfo` when the session is one (Go type-asserts it).
    // glue: interface method processinfoSetter.SetProcessInfo (D05.3)
    fn set_process_info(&mut self, sql: &str, t: SystemTime, command: u8, max_execution_time: u64);
    /// `sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp(sctx, plan)`: the MaxTS decision of an
    /// autocommit point get (`OptimisticTxnContextProvider.AdviseOptimizeWithPlan`) and the
    /// TSO prefetch.
    // glue: sessiontxn.AdviseOptimizeWithPlanAndThenWarmUp (D05.4, VS1-14)
    fn advise_optimize_with_plan_and_then_warm_up(&mut self, plan: &Plan) -> Result<(), Error>;
}

/// `resolve.ResultField`: one column of a result set, as the protocol layer describes it.
/// Go's `Column *model.ColumnInfo` and `Table *model.TableInfo` are reduced to the fields
/// `colNames2ResultFields` sets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResultField {
    /// `Column.Name`.
    pub column_name: CIStr,
    /// `Column.FieldType`.
    pub column_type: FieldType,
    pub column_as_name: CIStr,
    /// EmptyOrgName indicates whether this field has an empty org_name. A field has an empty
    /// org name, if it's an expression. It's not sure whether it's safe to use empty string
    /// in `.Column.Name`, so a new field is added to indicate whether it's empty.
    pub empty_org_name: bool,
    /// `Table.Name`.
    pub table_name: CIStr,
    pub table_as_name: CIStr,
    pub db_name: CIStr,
}

/// `plannercore.PlanCacheStmt.PointGet`: the point get executor cached with a prepared
/// statement (`PointGet.Executor`).
#[derive(Default)]
pub struct PlanCacheStmt {
    /// `PointGet.Executor`.
    pub point_get_executor: Option<Box<dyn Executor>>,
}

/// A prepared statement's cache entry, shared by the session's statement map and the
/// statements that execute it.
pub type PlanCacheStmtRef = Arc<parking_lot::Mutex<PlanCacheStmt>>;

/// The `*PointGetExecutor` methods `ExecStmt.PointGet` calls on a cached executor (D12.1,
/// VS1-12): Go type-asserts `exec.Executor` to `*PointGetExecutor`; here the point get
/// executor returns itself from [`Executor::as_point_get`].
pub trait PointGetExecutor {
    /// `PointGetExecutor.Init(p)`: re-initialises a cached executor for a new execution.
    // glue: Go method PointGetExecutor.Init (D12.1)
    fn init(&mut self, plan: &Plan) -> Result<(), Error>;
    /// `partitionDefIdx != nil`.
    // glue: Go field PointGetExecutor.partitionDefIdx (D12.1)
    fn is_partitioned(&self) -> bool;
}
